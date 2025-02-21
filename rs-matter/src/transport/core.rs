/*
 *
 *    Copyright (c) 2020-2022 Project CHIP Authors
 *
 *    Licensed under the Apache License, Version 2.0 (the "License");
 *    you may not use this file except in compliance with the License.
 *    You may obtain a copy of the License at
 *
 *        http://www.apache.org/licenses/LICENSE-2.0
 *
 *    Unless required by applicable law or agreed to in writing, software
 *    distributed under the License is distributed on an "AS IS" BASIS,
 *    WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *    See the License for the specific language governing permissions and
 *    limitations under the License.
 */

use core::borrow::Borrow;
use core::mem::MaybeUninit;
use core::pin::pin;

use embassy_futures::select::{select, select_slice, Either};
use embassy_sync::{blocking_mutex::raw::NoopRawMutex, channel::Channel};
use embassy_time::{Duration, Timer};

use log::{error, info, warn};

use crate::utils::select::Notification;
use crate::CommissioningData;
use crate::{
    alloc,
    data_model::{core::DataModel, objects::DataModelHandler},
    error::{Error, ErrorCode},
    interaction_model::core::PROTO_ID_INTERACTION_MODEL,
    secure_channel::{
        common::{OpCode, PROTO_ID_SECURE_CHANNEL},
        core::SecureChannel,
    },
    transport::packet::Packet,
    utils::select::EitherUnwrap,
    Matter,
};

use super::{
    exchange::{
        Exchange, ExchangeCtr, ExchangeCtx, ExchangeId, ExchangeState, Role, MAX_EXCHANGES,
    },
    mrp::ReliableMessage,
    packet::{MAX_RX_BUF_SIZE, MAX_RX_STATUS_BUF_SIZE, MAX_TX_BUF_SIZE},
    pipe::{Chunk, Pipe},
};

#[derive(Debug)]
enum OpCodeDescriptor {
    SecureChannel(OpCode),
    InteractionModel(crate::interaction_model::core::OpCode),
    Unknown(u8),
}

impl From<u8> for OpCodeDescriptor {
    fn from(value: u8) -> Self {
        if let Some(opcode) = num::FromPrimitive::from_u8(value) {
            Self::SecureChannel(opcode)
        } else if let Some(opcode) = num::FromPrimitive::from_u8(value) {
            Self::InteractionModel(opcode)
        } else {
            Self::Unknown(value)
        }
    }
}

type TxBuf = MaybeUninit<[u8; MAX_TX_BUF_SIZE]>;
type RxBuf = MaybeUninit<[u8; MAX_RX_BUF_SIZE]>;
type SxBuf = MaybeUninit<[u8; MAX_RX_STATUS_BUF_SIZE]>;

#[cfg(any(feature = "std", feature = "embassy-net"))]
pub struct RunBuffers {
    udp_bufs: crate::transport::udp::UdpBuffers,
    run_bufs: PacketBuffers,
    tx_buf: TxBuf,
    rx_buf: RxBuf,
}

#[cfg(any(feature = "std", feature = "embassy-net"))]
impl RunBuffers {
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            udp_bufs: crate::transport::udp::UdpBuffers::new(),
            run_bufs: PacketBuffers::new(),
            tx_buf: core::mem::MaybeUninit::uninit(),
            rx_buf: core::mem::MaybeUninit::uninit(),
        }
    }
}

pub struct PacketBuffers {
    tx: [TxBuf; MAX_EXCHANGES],
    rx: [RxBuf; MAX_EXCHANGES],
    sx: [SxBuf; MAX_EXCHANGES],
}

impl PacketBuffers {
    const TX_ELEM: TxBuf = MaybeUninit::uninit();
    const RX_ELEM: RxBuf = MaybeUninit::uninit();
    const SX_ELEM: SxBuf = MaybeUninit::uninit();

    const TX_INIT: [TxBuf; MAX_EXCHANGES] = [Self::TX_ELEM; MAX_EXCHANGES];
    const RX_INIT: [RxBuf; MAX_EXCHANGES] = [Self::RX_ELEM; MAX_EXCHANGES];
    const SX_INIT: [SxBuf; MAX_EXCHANGES] = [Self::SX_ELEM; MAX_EXCHANGES];

    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            tx: Self::TX_INIT,
            rx: Self::RX_INIT,
            sx: Self::SX_INIT,
        }
    }
}

impl<'a> Matter<'a> {
    #[cfg(any(feature = "std", feature = "embassy-net"))]
    pub async fn run<D, H>(
        &self,
        stack: &crate::transport::network::NetworkStack<D>,
        buffers: &mut RunBuffers,
        dev_comm: CommissioningData,
        handler: &H,
    ) -> Result<(), Error>
    where
        D: crate::transport::network::NetworkStackDriver,
        H: DataModelHandler,
    {
        let udp = crate::transport::udp::UdpListener::new(
            stack,
            crate::transport::network::SocketAddr::new(
                crate::transport::network::IpAddr::V6(
                    crate::transport::network::Ipv6Addr::UNSPECIFIED,
                ),
                self.port,
            ),
            &mut buffers.udp_bufs,
        )
        .await?;

        let tx_pipe = Pipe::new(unsafe { buffers.tx_buf.assume_init_mut() });
        let rx_pipe = Pipe::new(unsafe { buffers.rx_buf.assume_init_mut() });

        let tx_pipe = &tx_pipe;
        let rx_pipe = &rx_pipe;
        let udp = &udp;
        let run_bufs = &mut buffers.run_bufs;

        let mut tx = pin!(async move {
            loop {
                {
                    let mut data = tx_pipe.data.lock().await;

                    if let Some(chunk) = data.chunk {
                        udp.send(chunk.addr.unwrap_udp(), &data.buf[chunk.start..chunk.end])
                            .await?;
                        data.chunk = None;
                        tx_pipe.data_consumed_notification.signal(());
                    }
                }

                tx_pipe.data_supplied_notification.wait().await;
            }
        });

        let mut rx = pin!(async move {
            loop {
                {
                    let mut data = rx_pipe.data.lock().await;

                    if data.chunk.is_none() {
                        let (len, addr) = udp.recv(data.buf).await?;

                        data.chunk = Some(Chunk {
                            start: 0,
                            end: len,
                            addr: crate::transport::network::Address::Udp(addr),
                        });
                        rx_pipe.data_supplied_notification.signal(());
                    }
                }

                rx_pipe.data_consumed_notification.wait().await;
            }
        });

        let mut run = pin!(async move {
            self.run_piped(run_bufs, tx_pipe, rx_pipe, dev_comm, handler)
                .await
        });

        embassy_futures::select::select3(&mut tx, &mut rx, &mut run)
            .await
            .unwrap()
    }

    pub async fn run_piped<H>(
        &self,
        buffers: &mut PacketBuffers,
        tx_pipe: &Pipe<'_>,
        rx_pipe: &Pipe<'_>,
        dev_comm: CommissioningData,
        handler: &H,
    ) -> Result<(), Error>
    where
        H: DataModelHandler,
    {
        info!("Running Matter transport");

        let buf = unsafe { buffers.rx[0].assume_init_mut() };

        if self.start_comissioning(dev_comm, buf)? {
            info!("Comissioning started");
        }

        let construction_notification = Notification::new();

        let mut rx = pin!(self.handle_rx(buffers, rx_pipe, &construction_notification, handler));
        let mut tx = pin!(self.handle_tx(tx_pipe));

        select(&mut rx, &mut tx).await.unwrap()
    }

    #[inline(always)]
    async fn handle_rx<H>(
        &self,
        buffers: &mut PacketBuffers,
        rx_pipe: &Pipe<'_>,
        construction_notification: &Notification,
        handler: &H,
    ) -> Result<(), Error>
    where
        H: DataModelHandler,
    {
        info!("Creating queue for {} exchanges", 1);

        let channel = Channel::<NoopRawMutex, _, 1>::new();

        info!("Creating {} handlers", MAX_EXCHANGES);
        let mut handlers = heapless::Vec::<_, MAX_EXCHANGES>::new();

        info!("Handlers size: {}", core::mem::size_of_val(&handlers));

        // Unsafely allow mutable aliasing in the packet pools by different indices
        let pools: *mut PacketBuffers = buffers;

        for index in 0..MAX_EXCHANGES {
            let channel = &channel;
            let handler_id = index;

            let pools = unsafe { pools.as_mut() }.unwrap();

            let tx_buf = unsafe { pools.tx[handler_id].assume_init_mut() };
            let rx_buf = unsafe { pools.rx[handler_id].assume_init_mut() };
            let sx_buf = unsafe { pools.sx[handler_id].assume_init_mut() };

            handlers
                .push(self.exchange_handler(tx_buf, rx_buf, sx_buf, handler_id, channel, handler))
                .map_err(|_| ())
                .unwrap();
        }

        let mut rx = pin!(self.handle_rx_multiplex(rx_pipe, construction_notification, &channel));

        let result = select(&mut rx, select_slice(&mut handlers)).await;

        if let Either::First(result) = result {
            if let Err(e) = &result {
                error!("Exitting RX loop due to an error: {:?}", e);
            }

            result?;
        }

        Ok(())
    }

    #[inline(always)]
    pub async fn handle_tx(&self, tx_pipe: &Pipe<'_>) -> Result<(), Error> {
        loop {
            loop {
                {
                    let mut data = tx_pipe.data.lock().await;

                    if data.chunk.is_none() {
                        let mut tx = alloc!(Packet::new_tx(data.buf));

                        if self.pull_tx(&mut tx).await? {
                            data.chunk = Some(Chunk {
                                start: tx.get_writebuf()?.get_start(),
                                end: tx.get_writebuf()?.get_tail(),
                                addr: tx.peer,
                            });
                            tx_pipe.data_supplied_notification.signal(());
                        } else {
                            break;
                        }
                    }
                }

                tx_pipe.data_consumed_notification.wait().await;
            }

            self.wait_tx().await?;
        }
    }

    #[inline(always)]
    pub async fn handle_rx_multiplex<'t, 'e, const N: usize>(
        &'t self,
        rx_pipe: &Pipe<'_>,
        construction_notification: &'e Notification,
        channel: &Channel<NoopRawMutex, ExchangeCtr<'e>, N>,
    ) -> Result<(), Error>
    where
        't: 'e,
    {
        loop {
            info!("Transport: waiting for incoming packets");

            {
                let mut data = rx_pipe.data.lock().await;

                if let Some(chunk) = data.chunk {
                    let mut rx = alloc!(Packet::new_rx(&mut data.buf[chunk.start..chunk.end]));
                    rx.peer = chunk.addr;

                    if let Some(exchange_ctr) =
                        self.process_rx(construction_notification, &mut rx)?
                    {
                        let exchange_id = exchange_ctr.id().clone();

                        info!("Transport: got new exchange: {:?}", exchange_id);

                        channel.send(exchange_ctr).await;
                        info!("Transport: exchange sent");

                        self.wait_construction(construction_notification, &rx, &exchange_id)
                            .await?;

                        info!("Transport: exchange started");
                    }

                    data.chunk = None;
                    rx_pipe.data_consumed_notification.signal(());
                }
            }

            rx_pipe.data_supplied_notification.wait().await
        }

        #[allow(unreachable_code)]
        Ok::<_, Error>(())
    }

    #[inline(always)]
    pub async fn exchange_handler<const N: usize, H>(
        &self,
        tx_buf: &mut [u8; MAX_TX_BUF_SIZE],
        rx_buf: &mut [u8; MAX_RX_BUF_SIZE],
        sx_buf: &mut [u8; MAX_RX_STATUS_BUF_SIZE],
        handler_id: impl core::fmt::Display,
        channel: &Channel<NoopRawMutex, ExchangeCtr<'_>, N>,
        handler: &H,
    ) -> Result<(), Error>
    where
        H: DataModelHandler,
    {
        loop {
            let exchange_ctr: ExchangeCtr<'_> = channel.recv().await;

            info!(
                "Handler {}: Got exchange {:?}",
                handler_id,
                exchange_ctr.id()
            );

            let result = self
                .handle_exchange(tx_buf, rx_buf, sx_buf, exchange_ctr, handler)
                .await;

            if let Err(err) = result {
                warn!(
                    "Handler {}: Exchange closed because of error: {:?}",
                    handler_id, err
                );
            } else {
                info!("Handler {}: Exchange completed", handler_id);
            }
        }
    }

    #[inline(always)]
    #[cfg_attr(feature = "nightly", allow(clippy::await_holding_refcell_ref))] // Fine because of the async mutex
    pub async fn handle_exchange<H>(
        &self,
        tx_buf: &mut [u8; MAX_TX_BUF_SIZE],
        rx_buf: &mut [u8; MAX_RX_BUF_SIZE],
        sx_buf: &mut [u8; MAX_RX_STATUS_BUF_SIZE],
        exchange_ctr: ExchangeCtr<'_>,
        handler: &H,
    ) -> Result<(), Error>
    where
        H: DataModelHandler,
    {
        let mut tx = alloc!(Packet::new_tx(tx_buf.as_mut()));
        let mut rx = alloc!(Packet::new_rx(rx_buf.as_mut()));

        let mut exchange = alloc!(exchange_ctr.get(&mut rx).await?);

        match rx.get_proto_id() {
            PROTO_ID_SECURE_CHANNEL => {
                let sc = SecureChannel::new(self);

                sc.handle(&mut exchange, &mut rx, &mut tx).await?;

                self.notify_changed();
            }
            PROTO_ID_INTERACTION_MODEL => {
                let dm = DataModel::new(handler);

                let mut rx_status = alloc!(Packet::new_rx(sx_buf));

                dm.handle(&mut exchange, &mut rx, &mut tx, &mut rx_status)
                    .await?;

                self.notify_changed();
            }
            other => {
                error!("Unknown Proto-ID: {}", other);
            }
        }

        Ok(())
    }

    pub fn reset_transport(&self) {
        self.exchanges.borrow_mut().clear();
        self.session_mgr.borrow_mut().reset();
    }

    pub fn process_rx<'r>(
        &'r self,
        construction_notification: &'r Notification,
        src_rx: &mut Packet<'_>,
    ) -> Result<Option<ExchangeCtr<'r>>, Error> {
        self.purge()?;

        let mut exchanges = self.exchanges.borrow_mut();
        let (ctx, new) = match self.post_recv(&mut exchanges, src_rx) {
            Ok((ctx, new)) => (ctx, new),
            Err(e) => match e.code() {
                ErrorCode::Duplicate => {
                    self.send_notification.signal(());
                    return Ok(None);
                }
                _ => Err(e)?,
            },
        };

        src_rx.log("Got packet");

        if src_rx.proto.is_ack() {
            if new {
                Err(ErrorCode::Invalid)?;
            } else {
                let state = &mut ctx.state;

                match state {
                    ExchangeState::ExchangeRecv {
                        tx_acknowledged, ..
                    } => {
                        *tx_acknowledged = true;
                    }
                    ExchangeState::CompleteAcknowledge { notification, .. } => {
                        unsafe { notification.as_ref() }.unwrap().signal(());
                        ctx.state = ExchangeState::Closed;
                    }
                    _ => {
                        // TODO: Error handling
                        todo!()
                    }
                }

                self.notify_changed();
            }
        }

        if new {
            let constructor = ExchangeCtr {
                exchange: Exchange {
                    id: ctx.id.clone(),
                    matter: self,
                    notification: Notification::new(),
                },
                construction_notification,
            };

            self.notify_changed();

            Ok(Some(constructor))
        } else if src_rx.proto.proto_id == PROTO_ID_SECURE_CHANNEL
            && src_rx.proto.proto_opcode == OpCode::MRPStandAloneAck as u8
        {
            // Standalone ack, do nothing
            Ok(None)
        } else {
            let state = &mut ctx.state;

            match state {
                ExchangeState::ExchangeRecv {
                    rx, notification, ..
                } => {
                    let rx = unsafe { rx.as_mut() }.unwrap();
                    rx.load(src_rx)?;

                    unsafe { notification.as_ref() }.unwrap().signal(());
                    *state = ExchangeState::Active;
                }
                _ => {
                    // TODO: Error handling
                    todo!()
                }
            }

            self.notify_changed();

            Ok(None)
        }
    }

    pub async fn wait_construction(
        &self,
        construction_notification: &Notification,
        src_rx: &Packet<'_>,
        exchange_id: &ExchangeId,
    ) -> Result<(), Error> {
        construction_notification.wait().await;

        let mut exchanges = self.exchanges.borrow_mut();

        let ctx = ExchangeCtx::get(&mut exchanges, exchange_id).unwrap();

        let state = &mut ctx.state;

        match state {
            ExchangeState::Construction { rx, notification } => {
                let rx = unsafe { rx.as_mut() }.unwrap();
                rx.load(src_rx)?;

                unsafe { notification.as_ref() }.unwrap().signal(());
                *state = ExchangeState::Active;
            }
            _ => unreachable!(),
        }

        Ok(())
    }

    pub async fn wait_tx(&self) -> Result<(), Error> {
        select(
            self.send_notification.wait(),
            Timer::after(Duration::from_millis(100)),
        )
        .await;

        Ok(())
    }

    pub async fn pull_tx(&self, dest_tx: &mut Packet<'_>) -> Result<bool, Error> {
        self.purge()?;

        let mut exchanges = self.exchanges.borrow_mut();

        let ctx = exchanges.iter_mut().find(|ctx| {
            matches!(
                &ctx.state,
                ExchangeState::Acknowledge { .. }
                    | ExchangeState::ExchangeSend { .. }
                    // | ExchangeState::ExchangeRecv {
                    //     tx_acknowledged: false,
                    //     ..
                    // }
                    | ExchangeState::Complete { .. } // | ExchangeState::CompleteAcknowledge { .. }
            ) || ctx.mrp.is_ack_ready(*self.borrow())
        });

        if let Some(ctx) = ctx {
            self.notify_changed();

            let state = &mut ctx.state;

            let send = match state {
                ExchangeState::Acknowledge { notification } => {
                    ReliableMessage::prepare_ack(ctx.id.id, dest_tx);

                    unsafe { notification.as_ref() }.unwrap().signal(());
                    *state = ExchangeState::Active;

                    true
                }
                ExchangeState::ExchangeSend {
                    tx,
                    rx,
                    notification,
                } => {
                    let tx = unsafe { tx.as_ref() }.unwrap();
                    dest_tx.load(tx)?;

                    *state = ExchangeState::ExchangeRecv {
                        _tx: tx,
                        tx_acknowledged: false,
                        rx: *rx,
                        notification: *notification,
                    };

                    true
                }
                // ExchangeState::ExchangeRecv { .. } => {
                //     // TODO: Re-send the tx package if due
                //     false
                // }
                ExchangeState::Complete { tx, notification } => {
                    let tx = unsafe { tx.as_ref() }.unwrap();
                    dest_tx.load(tx)?;

                    *state = ExchangeState::CompleteAcknowledge {
                        _tx: tx as *const _,
                        notification: *notification,
                    };

                    true
                }
                // ExchangeState::CompleteAcknowledge { .. } => {
                //     // TODO: Re-send the tx package if due
                //     false
                // }
                _ => {
                    ReliableMessage::prepare_ack(ctx.id.id, dest_tx);
                    true
                }
            };

            if send {
                dest_tx.log("Sending packet");

                self.pre_send(ctx, dest_tx)?;
                self.notify_changed();

                return Ok(true);
            }
        }

        Ok(false)
    }

    fn purge(&self) -> Result<(), Error> {
        loop {
            let mut exchanges = self.exchanges.borrow_mut();

            if let Some(index) = exchanges.iter_mut().enumerate().find_map(|(index, ctx)| {
                matches!(ctx.state, ExchangeState::Closed).then_some(index)
            }) {
                exchanges.swap_remove(index);
            } else {
                break;
            }
        }

        Ok(())
    }

    fn post_recv<'r>(
        &self,
        exchanges: &'r mut heapless::Vec<ExchangeCtx, MAX_EXCHANGES>,
        rx: &mut Packet<'_>,
    ) -> Result<(&'r mut ExchangeCtx, bool), Error> {
        rx.plain_hdr_decode()?;

        // Get the session

        let mut session_mgr = self.session_mgr.borrow_mut();

        let sess_index = session_mgr.post_recv(rx)?;
        let session = session_mgr.mut_by_index(sess_index).unwrap();

        // Decrypt the message
        session.recv(self.epoch, rx)?;

        // Get the exchange
        // TODO: Handle out of space
        let (exch, new) = Self::register(
            exchanges,
            ExchangeId::load(rx),
            Role::complementary(rx.proto.is_initiator()),
            // We create a new exchange, only if the peer is the initiator
            rx.proto.is_initiator(),
        )?;

        // Message Reliability Protocol
        exch.mrp.recv(rx, self.epoch)?;

        Ok((exch, new))
    }

    fn pre_send(&self, ctx: &mut ExchangeCtx, tx: &mut Packet) -> Result<(), Error> {
        let mut session_mgr = self.session_mgr.borrow_mut();
        let sess_index = session_mgr
            .get(
                ctx.id.session_id.id,
                ctx.id.session_id.peer_addr,
                ctx.id.session_id.peer_nodeid,
                ctx.id.session_id.is_encrypted,
            )
            .ok_or(ErrorCode::NoSession)?;

        let session = session_mgr.mut_by_index(sess_index).unwrap();

        tx.proto.exch_id = ctx.id.id;
        if ctx.role == Role::Initiator {
            tx.proto.set_initiator();
        }

        session.pre_send(tx)?;
        ctx.mrp.pre_send(tx)?;
        session_mgr.send(sess_index, tx)
    }

    fn register(
        exchanges: &mut heapless::Vec<ExchangeCtx, MAX_EXCHANGES>,
        id: ExchangeId,
        role: Role,
        create_new: bool,
    ) -> Result<(&mut ExchangeCtx, bool), Error> {
        let exchange_index = exchanges
            .iter_mut()
            .enumerate()
            .find_map(|(index, exchange)| (exchange.id == id).then_some(index));

        if let Some(exchange_index) = exchange_index {
            let exchange = &mut exchanges[exchange_index];
            if exchange.role == role {
                Ok((exchange, false))
            } else {
                Err(ErrorCode::NoExchange.into())
            }
        } else if create_new {
            info!("Creating new exchange: {:?}", id);

            let exchange = ExchangeCtx {
                id,
                role,
                mrp: ReliableMessage::new(),
                state: ExchangeState::Active,
            };

            exchanges.push(exchange).map_err(|_| ErrorCode::NoSpace)?;

            Ok((exchanges.iter_mut().next_back().unwrap(), true))
        } else {
            Err(ErrorCode::NoExchange.into())
        }
    }
}

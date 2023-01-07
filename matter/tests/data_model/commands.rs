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

use matter::{
    data_model::{cluster_on_off, objects::EncodeValue},
    interaction_model::{
        core::{IMStatusCode, OpCode},
        messages::msg,
        messages::{
            ib::{CmdData, CmdPath, CmdStatus, InvResp},
            msg::InvReq,
        },
    },
    tlv::{self, FromTLV, TLVArray},
};

use crate::common::{echo_cluster, im_engine::im_engine};

enum ExpectedInvResp {
    Cmd(CmdPath, u8),
    Status(CmdStatus),
}

// Helper for handling Invoke Command sequences
fn handle_commands(input: &[CmdData], expected: &[ExpectedInvResp]) {
    let mut out_buf = [0u8; 400];
    let req = InvReq {
        suppress_response: Some(false),
        timed_request: Some(false),
        inv_requests: Some(TLVArray::Slice(input)),
    };

    let (_, _, out_buf) = im_engine(OpCode::InvokeRequest, &req, &mut out_buf);
    tlv::print_tlv_list(out_buf);
    let root = tlv::get_root_node_struct(out_buf).unwrap();

    let resp = msg::InvResp::from_tlv(&root).unwrap();
    let mut index = 0;
    for inv_response in resp.inv_responses.unwrap().iter() {
        println!("Validating index {}", index);
        match expected[index] {
            ExpectedInvResp::Cmd(e_c, e_d) => match inv_response {
                InvResp::Cmd(c) => {
                    assert_eq!(e_c, c.path);
                    match c.data {
                        EncodeValue::Tlv(t) => {
                            assert_eq!(e_d, t.find_tag(0).unwrap().u8().unwrap())
                        }
                        _ => panic!("Incorrect CmdDataType"),
                    }
                }
                _ => {
                    panic!("Invalid response, expected InvResponse::Cmd");
                }
            },
            ExpectedInvResp::Status(e_status) => match inv_response {
                InvResp::Status(status) => {
                    assert_eq!(e_status, status);
                }
                _ => {
                    panic!("Invalid response, expected InvResponse::Status");
                }
            },
        }
        println!("Index {} success", index);
        index += 1;
    }
    assert_eq!(index, expected.len());
}

macro_rules! cmd_data {
    ($path:ident, $data:literal) => {
        CmdData::new($path, EncodeValue::Value(&($data as u32)))
    };
}

macro_rules! echo_req {
    ($endpoint:literal, $data:literal) => {
        CmdData::new(
            CmdPath::new(
                Some($endpoint),
                Some(echo_cluster::ID),
                Some(echo_cluster::Commands::EchoReq as u16),
            ),
            EncodeValue::Value(&($data as u32)),
        )
    };
}

macro_rules! echo_resp {
    ($endpoint:literal, $data:literal) => {
        ExpectedInvResp::Cmd(
            CmdPath::new(
                Some($endpoint),
                Some(echo_cluster::ID),
                Some(echo_cluster::Commands::EchoResp as u16),
            ),
            $data,
        )
    };
}

#[test]
fn test_invoke_cmds_success() {
    // 2 echo Requests
    // - one on endpoint 0 with data 5,
    // - another on endpoint 1 with data 10
    let _ = env_logger::try_init();

    let input = &[echo_req!(0, 5), echo_req!(1, 10)];
    let expected = &[echo_resp!(0, 10), echo_resp!(1, 30)];
    handle_commands(input, expected);
}

#[test]
fn test_invoke_cmds_unsupported_fields() {
    // 5 commands
    // - endpoint doesn't exist - UnsupportedEndpoint
    // - cluster doesn't exist - UnsupportedCluster
    // - cluster doesn't exist and endpoint is wildcard - UnsupportedCluster
    // - command doesn't exist - UnsupportedCommand
    // - command doesn't exist and endpoint is wildcard - UnsupportedCommand
    let _ = env_logger::try_init();

    let invalid_endpoint = CmdPath::new(
        Some(2),
        Some(echo_cluster::ID),
        Some(echo_cluster::Commands::EchoReq as u16),
    );
    let invalid_cluster = CmdPath::new(
        Some(0),
        Some(0x1234),
        Some(echo_cluster::Commands::EchoReq as u16),
    );
    let invalid_cluster_wc_endpoint = CmdPath::new(
        None,
        Some(0x1234),
        Some(echo_cluster::Commands::EchoReq as u16),
    );
    let invalid_command = CmdPath::new(Some(0), Some(echo_cluster::ID), Some(0x1234));
    let invalid_command_wc_endpoint = CmdPath::new(None, Some(echo_cluster::ID), Some(0x1234));
    let input = &[
        cmd_data!(invalid_endpoint, 5),
        cmd_data!(invalid_cluster, 5),
        cmd_data!(invalid_cluster_wc_endpoint, 5),
        cmd_data!(invalid_command, 5),
        cmd_data!(invalid_command_wc_endpoint, 5),
    ];

    let expected = &[
        ExpectedInvResp::Status(CmdStatus::new(
            invalid_endpoint,
            IMStatusCode::UnsupportedEndpoint,
            0,
        )),
        ExpectedInvResp::Status(CmdStatus::new(
            invalid_cluster,
            IMStatusCode::UnsupportedCluster,
            0,
        )),
        ExpectedInvResp::Status(CmdStatus::new(
            invalid_command,
            IMStatusCode::UnsupportedCommand,
            0,
        )),
    ];
    handle_commands(input, expected);
}

#[test]
fn test_invoke_cmd_wc_endpoint_all_have_clusters() {
    // 1 echo Request with wildcard endpoint
    // should generate 2 responses from the echo clusters on both
    let _ = env_logger::try_init();
    let path = CmdPath::new(
        None,
        Some(echo_cluster::ID),
        Some(echo_cluster::Commands::EchoReq as u16),
    );
    let input = &[cmd_data!(path, 5)];
    let expected = &[echo_resp!(0, 10), echo_resp!(1, 15)];
    handle_commands(input, expected);
}

#[test]
fn test_invoke_cmd_wc_endpoint_only_1_has_cluster() {
    // 1 on command for on/off cluster with wildcard endpoint
    // should generate 1 response from the on-off cluster
    let _ = env_logger::try_init();

    let target = CmdPath::new(
        None,
        Some(cluster_on_off::ID),
        Some(cluster_on_off::Commands::On as u16),
    );
    let expected_path = CmdPath::new(
        Some(1),
        Some(cluster_on_off::ID),
        Some(cluster_on_off::Commands::On as u16),
    );
    let input = &[cmd_data!(target, 1)];
    let expected = &[ExpectedInvResp::Status(CmdStatus::new(
        expected_path,
        IMStatusCode::Sucess,
        0,
    ))];
    handle_commands(input, expected);
}

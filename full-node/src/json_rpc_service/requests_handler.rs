// Smoldot
// Copyright (C) 2023  Pierre Krieger
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

use smol::stream::StreamExt as _;
use smoldot::json_rpc::{methods, service};
use std::{future::Future, pin::Pin, sync::Arc};

pub struct Config {
    /// Function that can be used to spawn background tasks.
    ///
    /// The tasks passed as parameter must be executed until they shut down.
    pub tasks_executor: Arc<dyn Fn(Pin<Box<dyn Future<Output = ()> + Send>>) + Send + Sync>,

    pub receiver: async_channel::Receiver<Message>,
}

pub enum Message {
    Request(service::RequestProcess),
    SubscriptionStart(service::SubscriptionStartProcess),
}

pub fn spawn_requests_handler(mut config: Config) {
    (config.tasks_executor)(Box::pin(async move {
        loop {
            match config.receiver.next().await {
                Some(Message::Request(request)) => match request.request() {
                    methods::MethodCall::rpc_methods {} => {
                        request.respond(methods::Response::rpc_methods(methods::RpcMethods {
                            methods: methods::MethodCall::method_names()
                                .map(|n| n.into())
                                .collect(),
                        }));
                    }
                    methods::MethodCall::system_name {} => {
                        request.respond(methods::Response::system_version(
                            env!("CARGO_PKG_NAME").into(),
                        ));
                    }
                    methods::MethodCall::system_version {} => {
                        request.respond(methods::Response::system_version(
                            env!("CARGO_PKG_VERSION").into(),
                        ));
                    }
                    _ => request.fail(service::ErrorResponse::ServerError(
                        -32000,
                        "Not implemented in smoldot yet",
                    )),
                },
                Some(Message::SubscriptionStart(request)) => request.fail(
                    service::ErrorResponse::ServerError(-32000, "Not implemented in smoldot yet"),
                ),
                None => return,
            }
        }
    }));
}

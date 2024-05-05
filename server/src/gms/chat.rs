// SPDX-License-Identifier: MIT
// Copyright(c) 2024 Darek Stojaczyk

use anyhow::{anyhow, bail, Result};
use futures::FutureExt;
use log::trace;
use packet::pkt_common::ServiceID;
use packet::*;

use super::Connection;

pub struct GlobalChatHandler {
    conn: Connection,
}

impl std::fmt::Display for GlobalChatHandler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.conn)
    }
}

impl GlobalChatHandler {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub async fn handle(mut self) -> Result<()> {
        let conn_ref = self.conn.conn_ref.as_ref().unwrap().clone();
        let service = &conn_ref.service;

        #[rustfmt::skip]
        self.conn.stream
            .send(&Payload::ConnectAck(pkt_common::ConnectAck {
                bytes: BoundVec(vec![
                    0xff, 0xff, 0xff, 0x7f, 0, 0xff, 0, 0xff,
                    ServiceID::GlobalMgrSvr as u8, 0, 0, 0, 0,
                    service.world_id, service.channel_id, 0, 0, 0, 0, 0x1,
                ]),
            }))
            .await?;

        let p = self.conn.stream.recv().await?;
        let Payload::RegisterChatSvr(p) = p else {
            bail!("{self}: Expected RegisterChatSvr packet, got {p:?}");
        };
        trace!("{self}: Got {p:?}");

        let lender = conn_ref.borrower.inner::<GlobalChatHandler>();
        loop {
            futures::select! {
                p = self.conn.stream.recv().fuse() => {
                    let p = p.map_err(|e| {
                        anyhow!("{self}: Failed to recv a packet: {e}")
                    })?;
                    trace!("{self}: Got packet: {p:?}");
                }
                _ = lender.wait_to_lend().fuse() => {
                    lender.lend(&mut self).unwrap().await
                }
            }
        }
    }
}

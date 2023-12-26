// SPDX-License-Identifier: MIT
// Copyright(c) 2023 Darek Stojaczyk

use crate::packet_stream::PacketStream;
use aria::BlockExt;
use log::{debug, error, info, trace};
use packet::*;

use rand::Rng;
use std::cell::OnceCell;
use std::fmt::Display;
use std::os::fd::AsRawFd;
use std::{net::TcpListener, sync::Arc};

use anyhow::{bail, Context, Result};
use smol::Async;

#[derive(clap::Parser, Debug, Default)]
pub struct Args {}

pub struct Listener {
    tcp_listener: Async<TcpListener>,
    args: Arc<crate::args::Config>,
}

impl Listener {
    pub fn new(tcp_listener: Async<TcpListener>, args: Arc<crate::args::Config>) -> Self {
        Self { tcp_listener, args }
    }

    pub async fn listen(&mut self) -> Result<()> {
        info!(
            "Listener: started on {}",
            self.tcp_listener.get_ref().local_addr()?
        );

        loop {
            let (stream, _) = self.tcp_listener.accept().await?;

            let conn = Connection {
                id: stream.as_raw_fd(),
                stream: PacketStream::new(stream),
                shortkey: OnceCell::new(),
                args: self.args.clone(),
            };

            // Give the connection handler its own background task
            smol::spawn(async move {
                let id = conn.id;
                info!("Listener: new connection #{id}");
                if let Err(err) = conn.handle().await {
                    error!("Listener: connection #{id} error: {err}");
                }
                info!("Listener: closing connection #{id}");
            })
            .detach();
            // for now the tasks are just dropped, but we might want to
            // wait for them in the future (or send a special shutdown
            // message in each connection)
        }
    }
}

fn xor_blocks_mut(blocks: &mut [Block]) {
    blocks
        .iter_mut()
        .for_each(|c| c.iter_mut().for_each(|b| *b ^= 0xb3));
}

pub struct Connection {
    pub id: i32,
    pub stream: PacketStream,
    pub shortkey: OnceCell<aria::Key>,
    pub args: Arc<crate::args::Config>,
}

impl Display for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Conn #{}", self.id)
    }
}

impl Connection {
    pub async fn handle_key_req(&mut self, mut req: crypto_mgr::EncryptKey2Request) -> Result<()> {
        debug!(
            "{self}: key req key_split_point (w/o xor) = {:#x}",
            req.key_split_point
        );
        req.key_split_point ^= 0x1f398ab3;
        debug!(
            "{self}: key req key_split_point = {:#x}",
            req.key_split_point
        );

        let key = self.shortkey.get_or_init(|| {
            let mut rng = rand::thread_rng();
            let mut keybuf = [0u8; 32];
            (0..8).for_each(|i| {
                keybuf[i] = if rng.gen_bool(0.5) {
                    rng.gen_range(b'a'..=b'z')
                } else {
                    rng.gen_range(b'A'..=b'Z')
                };
            });
            aria::Key::from(keybuf)
        });

        debug!("{self}: key={key:x?}");
        let shortkey = &key.as_bytes()[0..9];
        let r = Payload::EncryptKey2Response(crypto_mgr::EncryptKey2Response {
            key_split_point: req.key_split_point,
            shortkey: UnboundVec(shortkey.iter().map(|b| b ^ 0xb3).collect()),
        });
        self.stream.send(&r).await
    }

    pub async fn handle_auth_req(&mut self, mut req: crypto_mgr::KeyAuthRequest) -> Result<()> {
        req.xor_port ^= 0x1f398ab3;
        debug!("{self}: auth req xor_port = {:x}", req.xor_port);

        assert_eq!(req.unk1, 0x0);
        assert_eq!(req.unk2, 0x0);

        let key = self.shortkey.get().context("shortkey not initialized")?;
        let enckey = key.expand();
        let deckey: aria::DecryptKey = enckey.clone().into();

        xor_blocks_mut(core::slice::from_mut(&mut req.ip_origin));
        deckey.decrypt_mut(&mut req.ip_origin);
        let ip_origin = &req.ip_origin;

        xor_blocks_mut(core::slice::from_mut(&mut req.ip_local));
        deckey.decrypt_mut(&mut req.ip_local);
        let ip_local = &req.ip_local;

        xor_blocks_mut(&mut req.srchash);
        req.srchash.iter_mut().for_each(|b| deckey.decrypt_mut(b));
        let srchash = req.srchash;

        xor_blocks_mut(&mut req.binbuf);
        req.binbuf.iter_mut().for_each(|b| deckey.decrypt_mut(b));
        let binbuf = req.binbuf;

        let ip_origin = ip_origin.try_as_str()?;
        let ip_local = ip_local.try_as_str()?;
        let srchash = srchash.try_as_str()?;
        let binbuf = binbuf.try_as_str()?;
        debug!("{self}: ip_origin={ip_origin}, ip_local={ip_local}, srchash={srchash}, binbuf={binbuf}");

        let ip_local = Block::new("127.0.0.1");
        let mut enc_item: [Block; 16] = Block::arr_from_slice("Data/Item.scp");
        enc_item.iter_mut().for_each(|b| enckey.encrypt_mut(b));
        xor_blocks_mut(&mut enc_item);
        let mut enc_mobs: [Block; 16] = Block::arr_from_slice("Data/Mobs.scp");
        enc_mobs.iter_mut().for_each(|b| enckey.encrypt_mut(b));
        xor_blocks_mut(&mut enc_mobs);
        let mut enc_warp: [Block; 16] = Block::arr_from_slice("Data/Warp.scp");
        enc_warp.iter_mut().for_each(|b| enckey.encrypt_mut(b));
        xor_blocks_mut(&mut enc_warp);

        let r = Payload::KeyAuthResponse(crypto_mgr::KeyAuthResponse {
            unk1: 0x1,
            xor_unk2: 0x03010101 ^ 0x1f398ab3,
            ip_local,
            xor_unk3: 4 ^ 0xb3,
            enc_item,
            xor_unk4: 2 ^ 0xb3,
            enc_mobs,
            xor_unk5: 1 ^ 0xb3,
            enc_warp,
            port: 38180,
        });
        self.stream.send(&r).await
    }

    pub async fn handle_esym(&mut self, esym: crypto_mgr::ESYM) -> Result<()> {
        let (req, len) = bincode::decode_from_slice::<crypto_mgr::ESYMRequest, _>(
            esym.bytes.0.as_slice(),
            bincode::config::legacy(),
        )?;
        if len != esym.bytes.0.len() {
            bail!("{self}: Trailing data in ESYM packet {:#?}", esym);
        }

        debug!(
            "{self}: ESYM req nation = {}, srchash = {}",
            req.nation.0, req.srchash.0
        );

        let path = self
            .args
            .resources_dir
            .join("resources/esym")
            .join(req.srchash.0)
            .with_extension("esym");
        let data = std::fs::read(&path).with_context(|| format!("cannot read {path:?}"))?;

        let r = crypto_mgr::ESYMResponse {
            unk1: 0x1,
            filesize: data.len() as u32,
            esym: UnboundVec(data),
        };

        let mut bytes = UnboundVec(vec![]);
        bincode::encode_into_std_write(r, &mut bytes.0, bincode::config::legacy())?;
        let r = Payload::ESYM(crypto_mgr::ESYM { bytes });
        self.stream.send(&r).await
    }

    pub async fn handle(mut self) -> Result<()> {
        let p = self.stream.recv().await?;
        let Payload::Connect(hello) = &p else {
            bail!("{self}: Expected Connect packet, got {p:?}");
        };
        let hello = packet::crypto_mgr::Connect::try_from(hello)?;

        assert_eq!(hello.unk1, 0xf6);
        assert_eq!(hello.world_id, 0xfd);

        trace!("{self}: Got hello: {p:?}");

        let ack = packet::crypto_mgr::ConnectAck {
            unk1: 0x0,
            unk2: [0x00, 0xff, 0x00, 0xff, 0x00, 0x00, 0x00, 0x00],
            unk3: 0xf6,
            unk4: 0xf6,
            unk5: 0x398ab300,
            unk6: 0x1f,
        };
        self.stream
            .send(&Payload::ConnectAck(ack.try_into()?))
            .await?;

        loop {
            let p = self.stream.recv().await?;
            match p {
                Payload::EncryptKey2Request(req) => self.handle_key_req(req).await?,
                Payload::KeyAuthRequest(req) => self.handle_auth_req(req).await?,
                Payload::ESYM(req) => self.handle_esym(req).await?,
                _ => {
                    trace!("{self}: Got packet: {p:?}");
                }
            }
        }
    }
}

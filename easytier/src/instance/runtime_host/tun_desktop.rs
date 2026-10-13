use std::{sync::Arc, time::Duration};

use anyhow::Context as _;
use easytier_core::{
    gateway::dhcp::{DhcpAddresses, DhcpHost},
    host::packet::HostPacketReceiver,
    instance::CorePacketPlane,
};
use futures::FutureExt as _;
use tokio::{
    sync::{Mutex, Notify, oneshot},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{
    MagicDnsRuntime,
    tun_common::{TunNicState, create_nic_ctx},
};
use crate::{
    common::{error::Error, global_ctx::ArcGlobalCtx},
    instance::shared_virtual_nic::ArcSharedVirtualNicRegistry,
};

pub(super) struct NativeTunRuntime {
    global_ctx: ArcGlobalCtx,
    cancel: CancellationToken,
    nic: TunNicState,
    shared_virtual_nic_registry: ArcSharedVirtualNicRegistry,
    static_ip_task: Mutex<Option<JoinHandle<()>>>,
}

impl NativeTunRuntime {
    pub(super) fn new(
        global_ctx: ArcGlobalCtx,
        cancel: CancellationToken,
        shared_virtual_nic_registry: ArcSharedVirtualNicRegistry,
    ) -> Self {
        Self {
            global_ctx,
            cancel,
            nic: TunNicState::empty(),
            shared_virtual_nic_registry,
            static_ip_task: Mutex::new(None),
        }
    }

    pub(super) fn install_packet_receiver(
        &self,
        receiver: HostPacketReceiver,
    ) -> anyhow::Result<()> {
        self.nic.install_receiver(receiver)
    }

    fn report_static_ip_cancelled(output: &mut Option<oneshot::Sender<Result<(), Error>>>) {
        if let Some(output) = output.take() {
            let _ = output.send(Err(anyhow::anyhow!(
                "instance is closing; static IP setup cancelled"
            )
            .into()));
        }
    }

    async fn start_static_ip(&self, packet_plane: Arc<CorePacketPlane>) -> anyhow::Result<()> {
        let ipv4 = self.global_ctx.get_ipv4();
        let ipv6 = self.global_ctx.get_ipv6();
        if ipv4.is_none() && ipv6.is_none() {
            return Ok(());
        }

        let nic_state = self.nic.clone();
        let cancel = self.cancel.clone();
        let global_ctx = self.global_ctx.clone();
        let receiver = self.nic.receiver();
        let shared_virtual_nic_registry = self.shared_virtual_nic_registry.clone();
        let (output, first_round) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut output = Some(output);
            loop {
                if cancel.is_cancelled() {
                    Self::report_static_ip_cancelled(&mut output);
                    return;
                }
                let closed = Arc::new(Notify::new());
                let mut nic = match create_nic_ctx(
                    global_ctx.clone(),
                    packet_plane.clone(),
                    receiver.clone(),
                    closed.clone(),
                    shared_virtual_nic_registry.clone(),
                )
                .await
                {
                    Ok(nic) => nic,
                    Err(error) => {
                        if let Some(output) = output.take() {
                            let _ = output.send(Err(error));
                            return;
                        }
                        tracing::error!(?error, "failed to create native interface context");
                        tokio::select! {
                            _ = cancel.cancelled() => return,
                            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        }
                        continue;
                    }
                };
                let result = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        Self::report_static_ip_cancelled(&mut output);
                        return;
                    }
                    result = nic.run(ipv4, ipv6) => result,
                };
                if let Err(error) = result {
                    if let Some(output) = output.take() {
                        let _ = output.send(Err(error));
                        return;
                    }
                    tracing::error!(?error, "failed to create native interface");
                    tokio::select! {
                        _ = cancel.cancelled() => return,
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    }
                    continue;
                }

                let magic_dns = if let Some(ip) = ipv4 {
                    let shared_route_backend = nic.shared_route_backend_for_dns();
                    MagicDnsRuntime::start(
                        global_ctx.clone(),
                        packet_plane.clone(),
                        nic.ifname().await,
                        ip,
                        shared_route_backend,
                    )
                } else {
                    MagicDnsRuntime::default()
                };
                nic_state.install(nic, magic_dns).await;
                if let Some(output) = output.take() {
                    let _ = output.send(Ok(()));
                }
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = closed.notified() => {}
                }
            }
        });
        self.static_ip_task.lock().await.replace(task);
        first_round
            .await
            .context("static IP setup task stopped")??;
        Ok(())
    }

    pub(super) async fn prepare(&self, packet_plane: Arc<CorePacketPlane>) -> anyhow::Result<()> {
        self.nic.drain().await;
        if !self.global_ctx.get_flags().no_tun
            && !self.global_ctx.config.get_dhcp()
            && !self.global_ctx.config.get_dhcpv6()
        {
            self.start_static_ip(packet_plane).await?;
        }
        Ok(())
    }

    pub(super) async fn shutdown(&self) {
        if let Some(task) = self.static_ip_task.lock().await.take() {
            let _ = task.await;
        }
        self.nic.stop().await;
    }

    pub(super) fn attach_fd(&self, _fd: i32) -> anyhow::Result<()> {
        anyhow::bail!("external TUN attachment is only supported on mobile Hosts")
    }

    pub(super) fn dhcp_host(
        &self,
        operation: Arc<Mutex<()>>,
        packet_plane: Arc<CorePacketPlane>,
    ) -> Arc<dyn DhcpHost> {
        Arc::new(NativeDhcpHost {
            global_ctx: self.global_ctx.clone(),
            operation,
            cancel: self.cancel.clone(),
            nic: self.nic.clone(),
            closed: Arc::new(Notify::new()),
            packet_plane,
            shared_virtual_nic_registry: self.shared_virtual_nic_registry.clone(),
        })
    }
}

pub(super) struct NativeDhcpHost {
    pub(super) global_ctx: ArcGlobalCtx,
    pub(super) operation: Arc<Mutex<()>>,
    pub(super) cancel: CancellationToken,
    nic: TunNicState,
    closed: Arc<Notify>,
    packet_plane: Arc<CorePacketPlane>,
    shared_virtual_nic_registry: ArcSharedVirtualNicRegistry,
}

impl NativeDhcpHost {
    pub(super) fn take_closed(&self) -> bool {
        self.closed.notified().now_or_never().is_some()
    }

    pub(super) async fn apply(&self, next: DhcpAddresses) -> anyhow::Result<()> {
        self.ensure_open()?;
        tokio::select! {
            _ = self.cancel.cancelled() => anyhow::bail!("instance is closing; DHCP update cancelled"),
            _ = self.nic.drain() => {}
        }
        self.set_addresses(DhcpAddresses::default());
        self.ensure_open()?;
        let addresses = self.with_static_addresses(next);
        if self.global_ctx.no_tun() || (addresses.ipv4.is_none() && addresses.ipv6.is_none()) {
            self.set_addresses(next);
            return Ok(());
        }

        let mut nic = create_nic_ctx(
            self.global_ctx.clone(),
            self.packet_plane.clone(),
            self.nic.receiver(),
            self.closed.clone(),
            self.shared_virtual_nic_registry.clone(),
        )
        .await?;
        tokio::select! {
            _ = self.cancel.cancelled() => anyhow::bail!("instance is closing; DHCP update cancelled"),
            result = nic.run(addresses.ipv4, addresses.ipv6) => result?,
        }
        let magic_dns = if let Some(ip) = addresses.ipv4 {
            let shared_route_backend = nic.shared_route_backend_for_dns();
            MagicDnsRuntime::start(
                self.global_ctx.clone(),
                self.packet_plane.clone(),
                nic.ifname().await,
                ip,
                shared_route_backend,
            )
        } else {
            MagicDnsRuntime::default()
        };
        self.nic.install(nic, magic_dns).await;
        self.set_addresses(next);
        Ok(())
    }
}

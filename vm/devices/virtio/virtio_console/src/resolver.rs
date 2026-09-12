// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Resource resolver for virtio-console devices.

use crate::VirtioConsoleDevice;
use async_trait::async_trait;
use serial_core::resources::ResolveSerialBackendParams;
use virtio::resolve::ResolvedVirtioDevice;
use virtio::resolve::VirtioResolveInput;
use virtio_resources::console::VirtioConsoleAttachmentMode;
use virtio_resources::console::VirtioConsoleHandle;
use virtio_resources::console::VirtioConsoleReconnectPolicy;
use vm_resource::AsyncResolveResource;
use vm_resource::ResourceResolver;
use vm_resource::declare_static_async_resolver;
use vm_resource::kind::VirtioDeviceHandle;

/// Resolver for virtio-console devices.
pub struct VirtioConsoleResolver;

declare_static_async_resolver! {
    VirtioConsoleResolver,
    (VirtioDeviceHandle, VirtioConsoleHandle),
}

#[async_trait]
impl AsyncResolveResource<VirtioDeviceHandle, VirtioConsoleHandle> for VirtioConsoleResolver {
    type Output = ResolvedVirtioDevice;
    type Error = anyhow::Error;

    async fn resolve(
        &self,
        resolver: &ResourceResolver,
        resource: VirtioConsoleHandle,
        input: VirtioResolveInput<'_>,
    ) -> Result<Self::Output, Self::Error> {
        resolve_console(
            resolver,
            input,
            resource.backend,
            resource.disconnect_policy,
            resource.attachment,
            "virtio-console",
        )
        .await
    }
}

async fn resolve_console(
    resolver: &ResourceResolver,
    input: VirtioResolveInput<'_>,
    backend: vm_resource::Resource<vm_resource::kind::SerialBackendHandle>,
    disconnect_policy: virtio_resources::console::VirtioConsoleDisconnectPolicy,
    attachment: Option<virtio_resources::console::VirtioConsoleAttachment>,
    worker_name: &'static str,
) -> anyhow::Result<ResolvedVirtioDevice> {
    validate_attachment(attachment.as_ref())?;
    let io = resolve_backend(resolver, &input, backend).await?;

    let device = VirtioConsoleDevice::new_with_name_and_policy(
        input.driver_source,
        io,
        worker_name,
        disconnect_policy,
    );

    Ok(device.into())
}

fn validate_attachment(
    attachment: Option<&virtio_resources::console::VirtioConsoleAttachment>,
) -> anyhow::Result<()> {
    if let Some(attachment) = attachment {
        anyhow::ensure!(
            !attachment.stable_id.is_empty() && attachment.stable_id.len() <= 128,
            "virtio-console attachment has an invalid stable ID"
        );
        anyhow::ensure!(
            !attachment.endpoint_identity.is_empty() && attachment.endpoint_identity.len() <= 4096,
            "virtio-console attachment has an invalid endpoint identity"
        );
        anyhow::ensure!(
            matches!(
                (attachment.mode, attachment.reconnect_policy),
                (
                    VirtioConsoleAttachmentMode::Listen,
                    VirtioConsoleReconnectPolicy::RecreateListener
                ) | (
                    VirtioConsoleAttachmentMode::Connect,
                    VirtioConsoleReconnectPolicy::ReconnectClient
                ) | (
                    VirtioConsoleAttachmentMode::Inherited,
                    VirtioConsoleReconnectPolicy::RequireInheritedAttachment
                ) | (
                    VirtioConsoleAttachmentMode::Inherited,
                    VirtioConsoleReconnectPolicy::DiscardWhileDisconnected
                )
            ),
            "virtio-console attachment mode and reconnect policy conflict"
        );
        match attachment.reconnect_policy {
            VirtioConsoleReconnectPolicy::RecreateListener => anyhow::ensure!(
                !attachment.required && attachment.reconnect_timeout_ms == 0,
                "listener console attachments must be optional and have no reconnect timeout"
            ),
            VirtioConsoleReconnectPolicy::ReconnectClient => anyhow::ensure!(
                attachment.required && attachment.reconnect_timeout_ms != 0,
                "client console attachments must be required with a bounded timeout"
            ),
            VirtioConsoleReconnectPolicy::RequireInheritedAttachment => anyhow::ensure!(
                attachment.required && attachment.reconnect_timeout_ms == 0,
                "inherited console attachments must be required with no reconnect timeout"
            ),
            VirtioConsoleReconnectPolicy::DiscardWhileDisconnected => anyhow::ensure!(
                !attachment.required && attachment.reconnect_timeout_ms == 0,
                "discarding console attachments must be optional with no reconnect timeout"
            ),
        }
    }
    Ok(())
}

async fn resolve_backend(
    resolver: &ResourceResolver,
    input: &VirtioResolveInput<'_>,
    backend: vm_resource::Resource<vm_resource::kind::SerialBackendHandle>,
) -> anyhow::Result<Box<dyn serial_core::SerialIo>> {
    let io = resolver
        .resolve(
            backend,
            ResolveSerialBackendParams {
                driver: Box::new(input.driver_source.simple()),
                _async_trait_workaround: &(),
            },
        )
        .await?;
    Ok(io.0.into_io())
}

use super::Imap;

use async_imap::extensions::idle::IdleResponse;
use async_imap::types::UnsolicitedResponse;
use async_std::prelude::*;
use std::time::{Duration, SystemTime};

use crate::error::{bail, format_err, Result};
use crate::{context::Context, scheduler::InterruptInfo};

use super::session::Session;

impl Imap {
    pub fn can_idle(&self) -> bool {
        self.config.can_idle
    }

    pub async fn idle(
        &mut self,
        context: &Context,
        watch_folder: Option<String>,
    ) -> Result<InterruptInfo> {
        use futures::future::FutureExt;

        if !self.can_idle() {
            bail!("IMAP server does not have IDLE capability");
        }
        self.setup_handle(context).await?;

        self.select_folder(context, watch_folder.clone()).await?;

        let timeout = Duration::from_secs(23 * 60);
        let mut info = Default::default();

        if let Some(session) = self.session.take() {
            // if we have unsolicited responses we directly return
            let mut unsolicited_exists = false;
            while let Ok(response) = session.unsolicited_responses.try_recv() {
                match response {
                    UnsolicitedResponse::Exists(_) => {
                        warn!(context, "skip idle, got unsolicited EXISTS {:?}", response);
                        unsolicited_exists = true;
                    }
                    _ => info!(context, "ignoring unsolicited response {:?}", response),
                }
            }

            if unsolicited_exists {
                self.session = Some(session);
                return Ok(info);
            }

            let mut handle = session.idle();
            if let Err(err) = handle.init().await {
                bail!("IMAP IDLE protocol failed to init/complete: {}", err);
            }

            let (idle_wait, interrupt) = handle.wait_with_timeout(timeout);

            enum Event {
                IdleResponse(IdleResponse),
                Interrupt(InterruptInfo),
            }

            info!(context, "Idle entering wait-on-remote state");
            let fut = idle_wait.map(|ev| ev.map(Event::IdleResponse)).race(async {
                let probe_network = self.idle_interrupt.recv().await;

                // cancel imap idle connection properly
                drop(interrupt);

                Ok(Event::Interrupt(probe_network.unwrap_or_default()))
            });

            match fut.await {
                Ok(Event::IdleResponse(IdleResponse::NewData(x))) => {
                    info!(context, "Idle has NewData {:?}", x);
                }
                Ok(Event::IdleResponse(IdleResponse::Timeout)) => {
                    info!(context, "Idle-wait timeout or interruption");
                }
                Ok(Event::IdleResponse(IdleResponse::ManualInterrupt)) => {
                    info!(context, "Idle wait was interrupted");
                }
                Ok(Event::Interrupt(i)) => {
                    info = i;
                    info!(context, "Idle wait was interrupted");
                }
                Err(err) => {
                    warn!(context, "Idle wait errored: {:?}", err);
                }
            }

            let session = handle
                .done()
                .timeout(Duration::from_secs(15))
                .await
                .map_err(|err| format_err!("IMAP IDLE protocol timed out: {}", err))??;
            self.session = Some(Session { inner: session });
        } else {
            warn!(context, "Attempted to idle without a session");
        }

        Ok(info)
    }

    pub(crate) async fn fake_idle(
        &mut self,
        context: &Context,
        watch_folder: Option<String>,
    ) -> InterruptInfo {
        // Idle using polling. This is also needed if we're not yet configured -
        // in this case, we're waiting for a configure job (and an interrupt).

        let fake_idle_start_time = SystemTime::now();

        // Do not poll, just wait for an interrupt when no folder is passed in.
        if watch_folder.is_none() {
            info!(context, "IMAP-fake-IDLE: no folder, waiting for interrupt");
            return self.idle_interrupt.recv().await.unwrap_or_default();
        }
        info!(context, "IMAP-fake-IDLEing folder={:?}", watch_folder);

        // check every minute if there are new messages
        // TODO: grow sleep durations / make them more flexible
        let mut interval = async_std::stream::interval(Duration::from_secs(60));

        enum Event {
            Tick,
            Interrupt(InterruptInfo),
        }
        // loop until we are interrupted or if we fetched something
        let info = loop {
            use futures::future::FutureExt;
            match interval
                .next()
                .map(|_| Event::Tick)
                .race(
                    self.idle_interrupt
                        .recv()
                        .map(|probe_network| Event::Interrupt(probe_network.unwrap_or_default())),
                )
                .await
            {
                Event::Tick => {
                    // try to connect with proper login params
                    // (setup_handle_if_needed might not know about them if we
                    // never successfully connected)
                    if let Err(err) = self.connect_configured(context).await {
                        warn!(context, "fake_idle: could not connect: {}", err);
                        continue;
                    }
                    if self.config.can_idle {
                        // we only fake-idled because network was gone during IDLE, probably
                        break InterruptInfo::new(false, None);
                    }
                    info!(context, "fake_idle is connected");
                    // we are connected, let's see if fetching messages results
                    // in anything.  If so, we behave as if IDLE had data but
                    // will have already fetched the messages so perform_*_fetch
                    // will not find any new.

                    if let Some(ref watch_folder) = watch_folder {
                        match self.fetch_new_messages(context, watch_folder, false).await {
                            Ok(res) => {
                                info!(context, "fetch_new_messages returned {:?}", res);
                                if res {
                                    break InterruptInfo::new(false, None);
                                }
                            }
                            Err(err) => {
                                error!(context, "could not fetch from folder: {}", err);
                                self.trigger_reconnect()
                            }
                        }
                    }
                }
                Event::Interrupt(info) => {
                    // Interrupt
                    break info;
                }
            }
        };

        info!(
            context,
            "IMAP-fake-IDLE done after {:.4}s",
            SystemTime::now()
                .duration_since(fake_idle_start_time)
                .unwrap_or_default()
                .as_millis() as f64
                / 1000.,
        );

        info
    }
}

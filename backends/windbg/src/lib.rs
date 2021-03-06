use async_trait::async_trait;
use futures::StreamExt;
use log::info;
use tokio::sync::mpsc::{Receiver, Sender};

use ghidradbg_backend::command::{
    dispatch_with, DebuggerCommandRequest, DebuggerCommandResponse, SingleStep,
};
use ghidradbg_backend::{Debugger, DebuggerError, DebuggerEvent, Result};

use crate::dbgeng::DebugClient;
use crate::dbgeng::DebugEventInterestFlags;

use tokio::sync::mpsc;
use tokio::time::timeout;
use winapi::_core::time::Duration;

mod dbgeng;
mod event;
mod handlers;

pub struct WinDbg {
    events: Receiver<DebuggerEvent>,
    command_requests: Sender<DebuggerCommandRequest>,
    command_responses: Receiver<DebuggerCommandResponse>,
    debugger_thread: Option<std::thread::JoinHandle<Result<()>>>,
    exit: tokio::sync::oneshot::Receiver<()>,
}

async fn windbg_event_loop(
    connection: String,
    mut events: Sender<DebuggerEvent>,
    mut command_requests: Receiver<DebuggerCommandRequest>,
    mut command_responses: Sender<DebuggerCommandResponse>,
) -> Result<()> {
    let mut client = DebugClient::connect(connection.as_str())?;
    let mut windbg_events = client.events(DebugEventInterestFlags::CHANGE_DEBUGGEE_STATE);

    info!("Connected to windbg successfully");

    loop {
        client.dispatch_callbacks()?;
        let read_timeout = Duration::from_millis(50);
        let next_event = timeout(read_timeout, windbg_events.next());
        let next_command = timeout(read_timeout, command_requests.next());

        tokio::select! {
            windbg_event = next_event => match windbg_event {
                Ok(Some(event)) => {
                    if let Some(translated_event) = event::translate(&mut client, event)? {
                        let _ = events.send(translated_event).await;
                    }
                },
                Ok(None) => break,
                _ => continue,
            },
            command_request = next_command => match command_request {
                Ok(Some(cmd)) => {
                    let response = dispatch_with(&mut client, cmd, |dispatcher| {
                        dispatcher.on::<SingleStep>(handlers::single_step)?;

                        Ok(())
                    })?;

                    if let Some(response) = response {
                        let _ = command_responses.send(response).await;
                    }
                },
                Ok(None) => break,
                _ => continue
            }
        }
    }
    Ok(())
}

#[async_trait]
impl Debugger for WinDbg {
    async fn launch(cli: String) -> Result<Self> {
        let (command_response_tx, command_response_rx) = mpsc::channel(1);
        let (command_tx, command_rx) = mpsc::channel(1);
        let (event_tx, event_rx) = mpsc::channel(10);
        let (exit_tx, exit_rx) = tokio::sync::oneshot::channel();

        let mut rt = tokio::runtime::Runtime::new()?;
        let cli = cli;

        // The WinDbg COM API MUST only be used on the same thread the object was created on.
        let debugger_thread = std::thread::spawn(move || {
            let res = rt.block_on(async move {
                let res = windbg_event_loop(cli, event_tx, command_rx, command_response_tx).await;
                let _ = exit_tx.send(());

                res
            });

            rt.shutdown_background();
            res
        });

        Ok(Self {
            events: event_rx,
            command_requests: command_tx,
            command_responses: command_response_rx,
            exit: exit_rx,
            debugger_thread: Some(debugger_thread),
        })
    }

    async fn handle_command(
        &mut self,
        request: DebuggerCommandRequest,
    ) -> Result<DebuggerCommandResponse> {
        let _ = self.command_requests.send(request).await;
        self.command_responses
            .recv()
            .await
            .ok_or(DebuggerError::TargetExited)
    }

    async fn next_event(&mut self) -> Result<DebuggerEvent> {
        let events = &mut self.events;
        let exit = &mut self.exit;

        if self.debugger_thread.is_none() {
            return Err(DebuggerError::TargetExited);
        }

        let res = tokio::select! {
            event = events.next() => {
                event.ok_or(DebuggerError::TargetExited)
            }
            _ = exit => {
                Err(DebuggerError::TargetExited)
            }
        };

        if let Err(e) = res {
            let debugger_thread = self.debugger_thread.take().unwrap();
            return Err(debugger_thread.join().unwrap().unwrap_err());
        }

        unimplemented!()
    }
}

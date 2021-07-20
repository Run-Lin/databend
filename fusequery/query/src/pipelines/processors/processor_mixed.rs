// Copyright 2020-2021 The Datafuse Authors.
//
// SPDX-License-Identifier: Apache-2.0.

use std::any::Any;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use common_datablocks::DataBlock;
use common_exception::ErrorCode;
use common_exception::Result;
use common_infallible::RwLock;
use common_runtime::tokio::sync::mpsc;
use common_streams::SendableDataBlockStream;
use log::error;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::StreamExt;

use crate::pipelines::processors::Processor;
use crate::sessions::FuseQueryContextRef;

// M inputs--> N outputs Mixed processor
struct MixedWorker {
    ctx: FuseQueryContextRef,
    inputs: Vec<Arc<dyn Processor>>,
    n: usize,
    shared_num: AtomicUsize,
    started: AtomicBool,
    receivers: Vec<Option<mpsc::Receiver<Result<DataBlock>>>>,
}

impl MixedWorker {
    pub fn prepare_inputstream(&self) -> Result<SendableDataBlockStream> {
        let inputs = self.inputs.len();
        match inputs {
            0 => Result::Err(ErrorCode::IllegalTransformConnectionState(
                "Mixed processor inputs cannot be zero",
            )),
            _ => {
                let (sender, receiver) = mpsc::channel::<Result<DataBlock>>(inputs);
                for i in 0..inputs {
                    let input = self.inputs[i].clone();
                    let sender = sender.clone();
                    self.ctx.execute_task(async move {
                        let mut stream = match input.execute().await {
                            Err(e) => {
                                if let Err(error) = sender.send(Result::Err(e)).await {
                                    error!("Mixed processor cannot push data: {}", error);
                                }
                                return;
                            }
                            Ok(stream) => stream,
                        };

                        while let Some(item) = stream.next().await {
                            match item {
                                Ok(item) => {
                                    if let Err(error) = sender.send(Ok(item)).await {
                                        // Stop pulling data
                                        error!("Mixed processor cannot push data: {}", error);
                                        return;
                                    }
                                }
                                Err(error) => {
                                    // Stop pulling data
                                    if let Err(error) = sender.send(Err(error)).await {
                                        error!("Mixed processor cannot push data: {}", error);
                                    }
                                    return;
                                }
                            }
                        }
                    })?;
                }
                Ok(Box::pin(ReceiverStream::new(receiver)))
            }
        }
    }

    pub fn start(&mut self) -> Result<()> {
        if self.started.load(Ordering::Relaxed) {
            return Ok(());
        }

        let inputs = self.inputs.len();
        let outputs = self.n;

        let mut senders = Vec::with_capacity(outputs);
        for _i in 0..self.n {
            let (sender, receiver) = mpsc::channel::<Result<DataBlock>>(inputs);
            senders.push(sender);
            self.receivers.push(Some(receiver));
        }

        let mut stream = self.prepare_inputstream()?;
        self.ctx.execute_task(async move {
            let index = AtomicUsize::new(0);
            while let Some(item) = stream.next().await {
                let i = index.fetch_add(1, Ordering::Relaxed) % outputs;
                // TODO: USE try_reserve when the channel is blocking
                if let Err(error) = senders[i].send(item).await {
                    error!("Mixed processor cannot push data: {}", error);
                }
            }
        })?;

        self.started.store(true, Ordering::Relaxed);
        Ok(())
    }
}

pub struct MixedProcessor {
    worker: Arc<RwLock<MixedWorker>>,
    index: usize,
}

impl MixedProcessor {
    pub fn create(ctx: FuseQueryContextRef, n: usize) -> Self {
        let worker = MixedWorker {
            ctx,
            inputs: vec![],
            n,
            started: AtomicBool::new(false),
            shared_num: AtomicUsize::new(0),
            receivers: vec![],
        };

        let index = worker.shared_num.fetch_add(1, Ordering::Relaxed);
        Self {
            worker: Arc::new(RwLock::new(worker)),
            index,
        }
    }

    pub fn share(&self) -> Result<Self> {
        let worker = self.worker.read();
        let index = worker.shared_num.fetch_add(1, Ordering::Relaxed);
        if index >= worker.n {
            return Err(ErrorCode::LogicalError("Mixed shared num overflow"));
        }

        Ok(Self {
            worker: self.worker.clone(),
            index,
        })
    }
}

#[async_trait::async_trait]
impl Processor for MixedProcessor {
    fn name(&self) -> &str {
        "MixedProcessor"
    }

    fn connect_to(&mut self, input: Arc<dyn Processor>) -> Result<()> {
        let mut worker = self.worker.write();
        worker.inputs.push(input);
        Ok(())
    }

    fn inputs(&self) -> Vec<Arc<dyn Processor>> {
        let worker = self.worker.read();
        worker.inputs.clone()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    async fn execute(&self) -> Result<SendableDataBlockStream> {
        let receiver = {
            let mut worker = self.worker.write();
            worker.start()?;
            worker.receivers[self.index].take()
        }
        .unwrap();

        Ok(Box::pin(ReceiverStream::new(receiver)))
    }
}

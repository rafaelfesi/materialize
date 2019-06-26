// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

//! An interactive dataflow server.

use differential_dataflow::trace::cursor::Cursor;
use differential_dataflow::trace::TraceReader;

use timely::communication::initialize::WorkerGuards;
use timely::communication::Allocate;
use timely::progress::frontier::Antichain;
use timely::synchronization::sequence::Sequencer;
use timely::worker::Worker as TimelyWorker;

use lazy_static::lazy_static;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::render;
use super::render::InputCapability;
use super::trace::{KeysOnlyHandle, TraceManager};
use super::RelationExpr;
use super::{LocalSourceConnector, Source, SourceConnector};
use crate::dataflow::{Dataflow, Timestamp, View};
use crate::glue::*;
use crate::repr::{ColumnType, Datum, RelationType, ScalarType};

pub fn serve(
    dataflow_command_receivers: Vec<UnboundedReceiver<(DataflowCommand, CommandMeta)>>,
    peek_results_handler: PeekResultsHandler,
    num_workers: usize,
) -> Result<WorkerGuards<()>, String> {
    assert_eq!(dataflow_command_receivers.len(), num_workers);
    // wrap up receivers so individual workers can take them
    let dataflow_command_receivers = Arc::new(Mutex::new(
        dataflow_command_receivers
            .into_iter()
            .map(Some)
            .collect::<Vec<_>>(),
    ));

    timely::execute(timely::Configuration::Process(num_workers), move |worker| {
        let dataflow_command_receivers = dataflow_command_receivers.clone();
        let dataflow_command_receiver = {
            dataflow_command_receivers.lock().unwrap()[worker.index()]
                .take()
                .unwrap()
        };
        Worker::new(
            worker,
            dataflow_command_receiver,
            peek_results_handler.clone(),
        )
        .run()
    })
}

#[derive(Clone)]
pub enum PeekResultsHandler {
    Local(PeekResultsMux),
    Remote,
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingPeek {
    /// The expr that identifies the dataflow to peek.
    expr: RelationExpr,
    /// Identifies intended recipient of the peek.
    connection_uuid: uuid::Uuid,
    /// Time at which the collection should be materialized.
    timestamp: Timestamp,
    /// Handle to trace.
    drop_after_peek: Option<Dataflow>,
}

lazy_static! {
    // Bootstrapping adds a dummy table, "dual", with one row, which the SQL
    // planner depends upon.
    //
    // TODO(benesch): perhaps the SQL layer should be responsible for installing
    // it, then? It's not fundamental to this module.
    static ref BOOTSTRAP_COMMANDS: Vec<DataflowCommand> = vec![
        DataflowCommand::CreateDataflow(Dataflow::Source(Source {
            name: "dual".into(),
            connector: SourceConnector::Local(LocalSourceConnector {}),
            typ: RelationType::new(vec![ColumnType {
                name: Some("x".into()),
                nullable: false,
                scalar_type: ScalarType::String,
            }]),
        })),
        DataflowCommand::Insert("dual".into(), vec![vec![Datum::String("X".into())]]),
    ];
}

struct Worker<'w, A>
where
    A: Allocate,
{
    inner: &'w mut TimelyWorker<A>,
    dataflow_command_receiver: UnboundedReceiver<(DataflowCommand, CommandMeta)>,
    peek_results_handler: PeekResultsHandler,
    pending_peeks: Vec<(PendingPeek, KeysOnlyHandle)>,
    traces: TraceManager,
    rpc_client: reqwest::Client,
    inputs: HashMap<String, InputCapability>,
    input_time: u64,
    transient_view_counter: u64,
    dataflows: HashMap<String, Dataflow>,
    sequencer: Sequencer<PendingPeek>,
}

impl<'w, A> Worker<'w, A>
where
    A: Allocate,
{
    fn new(
        w: &'w mut TimelyWorker<A>,
        dataflow_command_receiver: UnboundedReceiver<(DataflowCommand, CommandMeta)>,
        peek_results_handler: PeekResultsHandler,
    ) -> Worker<'w, A> {
        let sequencer = Sequencer::new(w, Instant::now());
        Worker {
            inner: w,
            dataflow_command_receiver,
            peek_results_handler,
            pending_peeks: Vec::new(),
            traces: TraceManager::new(),
            rpc_client: reqwest::Client::new(),
            inputs: HashMap::new(),
            input_time: 1,
            transient_view_counter: 1,
            dataflows: HashMap::new(),
            sequencer,
        }
    }

    /// Draws from `dataflow_command_receiver` until shutdown.
    fn run(&mut self) {
        for cmd in BOOTSTRAP_COMMANDS.iter() {
            self.handle_command(
                cmd.clone(),
                CommandMeta {
                    connection_uuid: Uuid::nil(),
                },
            );
        }

        let mut shutdown = false;
        while !shutdown {
            // Ask Timely to execute a unit of work.
            // Can either yield tastefully, or busy-wait.
            // self.inner.step_or_park(None);
            self.inner.step();

            self.process_peeks();

            // Handle any received commands
            while let Ok(Some((cmd, cmd_meta))) = self.dataflow_command_receiver.try_next() {
                if let DataflowCommand::Shutdown = cmd {
                    shutdown = true;
                }
                self.handle_command(cmd, cmd_meta);
            }
        }
    }

    fn handle_command(&mut self, cmd: DataflowCommand, cmd_meta: CommandMeta) {
        match cmd {
            DataflowCommand::CreateDataflow(dataflow) => {
                render::build_dataflow(
                    &dataflow,
                    &mut self.traces,
                    self.inner,
                    &mut self.inputs,
                    self.input_time,
                );
                self.dataflows.insert(dataflow.name().to_owned(), dataflow);
            }

            DataflowCommand::DropDataflows(dataflows) => {
                for dataflow in dataflows {
                    self.inputs.remove(dataflow.name());
                    self.dataflows.remove(dataflow.name());
                    if let Dataflow::Sink { .. } = dataflow {
                        // TODO(jamii) it's not clear how we're supposed to drop a Sink
                    } else {
                        self.traces.del_trace(&RelationExpr::Get {
                            name: dataflow.name().to_owned(),
                            typ: dataflow.typ().clone(),
                        });
                    }
                }
            }

            DataflowCommand::PeekExisting { dataflow, when } => {
                self.sequence_peek(cmd_meta, dataflow, when, false /* drop */)
            }

            DataflowCommand::PeekTransient {
                relation_expr,
                when,
            } => {
                let typ = relation_expr.typ();
                let dataflow = Dataflow::View(View {
                    name: format!("<temp_{}>", self.transient_view_counter),
                    relation_expr,
                    typ,
                });
                render::build_dataflow(
                    &dataflow,
                    &mut self.traces,
                    self.inner,
                    &mut self.inputs,
                    self.input_time,
                );
                self.dataflows
                    .insert(dataflow.name().to_owned(), dataflow.clone());
                self.sequence_peek(cmd_meta, dataflow, when, true /* drop */);
                self.transient_view_counter += 1;
            }

            DataflowCommand::Insert(name, rows) => {
                // Only broadcast the input on the first worker. Otherwise we'd
                // insert multiple copies.
                if self.inner.index() == 0 {
                    match self.inputs.get_mut(&name).expect("Failed to find input") {
                        InputCapability::Local { handle, capability } => {
                            let mut session = handle.session(capability.clone());
                            for row in rows {
                                session.give((row, self.input_time, 1));
                            }
                        }
                        InputCapability::External(_) => {
                            panic!("attempted to insert into external source")
                        }
                    };
                }

                // Unconditionally advance time after an insertion to allow the
                // computation to make progress. Importantly, this occurs on
                // *all* internal inputs.
                self.input_time += 1;
                for handle in self.inputs.values_mut() {
                    match handle {
                        InputCapability::Local { capability, .. } => {
                            capability.downgrade(&self.input_time);
                        }
                        InputCapability::External(_) => (),
                    }
                }
            }

            DataflowCommand::Tail(_) => unimplemented!(),

            DataflowCommand::Shutdown => {
                // this should lead timely to wind down eventually
                self.inputs.clear();
                self.traces.del_all_traces();
            }
        }
    }

    fn sequence_peek(
        &mut self,
        cmd_meta: CommandMeta,
        dataflow: Dataflow,
        when: PeekWhen,
        drop: bool,
    ) {
        if self.inner.index() != 0 {
            // Only worker 0 sequences peeks. It will broadcast the sequenced
            // peek to other workers.
            return;
        }

        let get = RelationExpr::Get {
            name: dataflow.name().to_owned(),
            typ: dataflow.typ().clone(),
        };

        let timestamp = match when {
            PeekWhen::Immediately => {
                let trace = self.traces.get_trace(&get).unwrap_or_else(|| {
                    panic!("failed to find arrangement for PEEK {}", dataflow.name())
                });

                // Ask the trace for the latest time it knows about. The latest
                // "committed" time (i.e., the time at which we know results can
                // never change) is one less than that.
                //
                // TODO(benesch, fms): this approach does not work for arbitrary
                // timestamp types, where a predecessor operation may not exist.
                let mut upper = Antichain::new();
                trace.clone().read_upper(&mut upper);
                if upper.elements().is_empty() {
                    0
                } else {
                    assert_eq!(upper.elements().len(), 1);
                    upper.elements()[0].saturating_sub(1)
                }
            }

            // Compute the lastest time that is committed by all inputs. Peeking
            // at this time may involve waiting for the outputs to catch up.
            PeekWhen::AfterFlush => self.root_input_time(dataflow.name()).saturating_sub(1),

            PeekWhen::AtTimestamp(timestamp) => timestamp,
        };

        self.sequencer.push(PendingPeek {
            expr: get,
            connection_uuid: cmd_meta.connection_uuid,
            timestamp,
            drop_after_peek: if drop { Some(dataflow) } else { None },
        })
    }

    fn root_input_time(&self, name: &str) -> u64 {
        match &self.dataflows[name] {
            Dataflow::Source(_) => match &self.inputs[name] {
                InputCapability::External(capability) => *capability.borrow().time(),
                InputCapability::Local { capability, .. } => *capability.time(),
            },
            Dataflow::Sink(_) => unreachable!(),
            v @ Dataflow::View(_) => v
                .uses()
                .iter()
                .map(|n| self.root_input_time(n))
                .min()
                .unwrap_or(u64::max_value()),
        }
    }

    /// Scan pending peeks and attempt to retire each.
    fn process_peeks(&mut self) {
        // Look for new peeks.
        while let Some(pending_peek) = self.sequencer.next() {
            let trace = self.traces.get_trace(&pending_peek.expr).unwrap();
            self.pending_peeks.push((pending_peek, trace));
        }

        // See if time has advanced enough to handle any of our pending
        // peeks.
        let mut dataflows_to_be_dropped = vec![];
        {
            let Worker {
                pending_peeks,
                peek_results_handler,
                rpc_client,
                ..
            } = self;
            pending_peeks.retain(|(peek, trace)| {
                let mut upper = timely::progress::frontier::Antichain::new();
                let mut trace = trace.clone();
                trace.read_upper(&mut upper);

                // To produce output at `peek.timestamp`, we must be certain that
                // it is no longer changing. A trace guarantees that all future
                // changes will be greater than or equal to an element of `upper`.
                //
                // If an element of `upper` is less or equal to `peek.timestamp`,
                // then there can be further updates that would change the output.
                // If no element of `upper` is less or equal to `peek.timestamp`,
                // then for any time `t` less or equal to `peek.timestamp` it is
                // not the case that `upper` is less or equal to that timestamp,
                // and so the result cannot further evolve.
                if upper.less_equal(&peek.timestamp) {
                    return true; // retain
                }
                let (mut cur, storage) = trace.cursor();
                let mut results = Vec::new();
                while let Some(key) = cur.get_key(&storage) {
                    // TODO: Absent value iteration might be weird (in principle
                    // the cursor *could* say no `()` values associated with the
                    // key, though I can't imagine how that would happen for this
                    // specific trace implementation).

                    let mut copies = 0;
                    cur.map_times(&storage, |time, diff| {
                        use timely::order::PartialOrder;
                        if time.less_equal(&peek.timestamp) {
                            copies += diff;
                        }
                    });
                    assert!(copies >= 0);
                    for _ in 0..copies {
                        results.push(key.clone());
                    }

                    cur.step_key(&storage)
                }
                match peek_results_handler {
                    PeekResultsHandler::Local(peek_results_mux) => {
                        // The sender is allowed disappear at any time, so the
                        // error handling here is deliberately relaxed.
                        if let Ok(sender) = peek_results_mux
                            .read()
                            .unwrap()
                            .sender(&peek.connection_uuid)
                        {
                            drop(sender.unbounded_send(results))
                        }
                    }
                    PeekResultsHandler::Remote => {
                        let encoded = bincode::serialize(&results).unwrap();
                        rpc_client
                            .post("http://localhost:6875/api/peek-results")
                            .header("X-Materialize-Query-UUID", peek.connection_uuid.to_string())
                            .body(encoded)
                            .send()
                            .unwrap();
                    }
                }
                if let Some(dataflow) = &peek.drop_after_peek {
                    dataflows_to_be_dropped.push(dataflow.clone());
                }
                false // don't retain
            });
        }
        if !dataflows_to_be_dropped.is_empty() {
            self.handle_command(
                DataflowCommand::DropDataflows(dataflows_to_be_dropped),
                CommandMeta {
                    connection_uuid: Uuid::nil(),
                },
            );
        }
    }
}

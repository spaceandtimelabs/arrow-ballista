// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::any::type_name;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Instant;

use crate::scheduler_server::event::QueryStageSchedulerEvent;
use crate::scheduler_server::SessionBuilder;
use crate::state::backend::{Lock, StateBackendClient};
use crate::state::executor_manager::{ExecutorManager, ExecutorReservation};
use crate::state::session_manager::SessionManager;
use crate::state::task_manager::TaskManager;

use ballista_core::error::{BallistaError, Result};
use ballista_core::serde::protobuf::TaskStatus;
use ballista_core::serde::{AsExecutionPlan, BallistaCodec};
use datafusion::logical_plan::LogicalPlan;
use datafusion::prelude::SessionContext;
use datafusion_proto::logical_plan::AsLogicalPlan;
use log::{debug, error, info};
use prost::Message;
use datafusion::datasource::datasource::TableProviderFactory;

pub mod backend;
pub mod execution_graph;
pub mod executor_manager;
pub mod session_manager;
pub mod session_registry;
mod task_manager;

pub fn decode_protobuf<T: Message + Default>(bytes: &[u8]) -> Result<T> {
    T::decode(bytes).map_err(|e| {
        BallistaError::Internal(format!(
            "Could not deserialize {}: {}",
            type_name::<T>(),
            e
        ))
    })
}

pub fn decode_into<T: Message + Default + Into<U>, U>(bytes: &[u8]) -> Result<U> {
    T::decode(bytes)
        .map_err(|e| {
            BallistaError::Internal(format!(
                "Could not deserialize {}: {}",
                type_name::<T>(),
                e
            ))
        })
        .map(|t| t.into())
}

pub fn encode_protobuf<T: Message + Default>(msg: &T) -> Result<Vec<u8>> {
    let mut value: Vec<u8> = Vec::with_capacity(msg.encoded_len());
    msg.encode(&mut value).map_err(|e| {
        BallistaError::Internal(format!(
            "Could not serialize {}: {}",
            type_name::<T>(),
            e
        ))
    })?;
    Ok(value)
}

#[derive(Clone)]
pub(super) struct SchedulerState<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan>
{
    pub executor_manager: ExecutorManager,
    pub task_manager: TaskManager<T, U>,
    pub session_manager: SessionManager,
    pub codec: BallistaCodec<T, U>,
}

impl<T: 'static + AsLogicalPlan, U: 'static + AsExecutionPlan> SchedulerState<T, U> {
    #[cfg(test)]
    pub fn new_with_default_scheduler_name(
        config_client: Arc<dyn StateBackendClient>,
        session_builder: SessionBuilder,
        codec: BallistaCodec<T, U>,
    ) -> Self {
        SchedulerState::new(
            config_client,
            session_builder,
            codec,
            "localhost:50050".to_owned(),
            HashMap::default(),
        )
    }

    pub fn new(
        config_client: Arc<dyn StateBackendClient>,
        session_builder: SessionBuilder,
        codec: BallistaCodec<T, U>,
        scheduler_name: String,
        table_factories: HashMap<String, Arc<dyn TableProviderFactory>>,
    ) -> Self {
        Self {
            executor_manager: ExecutorManager::new(config_client.clone()),
            task_manager: TaskManager::new(
                config_client.clone(),
                session_builder,
                codec.clone(),
                scheduler_name,
                table_factories.clone(),
            ),
            session_manager: SessionManager::new(config_client, session_builder, table_factories),
            codec,
        }
    }

    pub async fn init(&self) -> Result<()> {
        self.executor_manager.init().await
    }

    #[cfg(not(test))]
    pub(crate) async fn update_task_statuses(
        &self,
        executor_id: &str,
        tasks_status: Vec<TaskStatus>,
    ) -> Result<(Vec<QueryStageSchedulerEvent>, Vec<ExecutorReservation>)> {
        let executor = self
            .executor_manager
            .get_executor_metadata(executor_id)
            .await?;

        let total_num_tasks = tasks_status.len();
        let reservations = (0..total_num_tasks)
            .into_iter()
            .map(|_| ExecutorReservation::new_free(executor_id.to_owned()))
            .collect();

        let events = self
            .task_manager
            .update_task_statuses(&executor, tasks_status)
            .await?;

        Ok((events, reservations))
    }

    #[cfg(test)]
    pub(crate) async fn update_task_statuses(
        &self,
        executor_id: &str,
        tasks_status: Vec<TaskStatus>,
    ) -> Result<(Vec<QueryStageSchedulerEvent>, Vec<ExecutorReservation>)> {
        let executor = self
            .executor_manager
            .get_executor_metadata(executor_id)
            .await?;

        let total_num_tasks = tasks_status.len();
        let free_list = (0..total_num_tasks)
            .into_iter()
            .map(|_| ExecutorReservation::new_free(executor_id.to_owned()))
            .collect();

        let events = self
            .task_manager
            .update_task_statuses(&executor, tasks_status)
            .await?;

        self.executor_manager.cancel_reservations(free_list).await?;

        Ok((events, vec![]))
    }

    /// Process reservations which are offered. The basic process is
    /// 1. Attempt to fill the offered reservations with available tasks
    /// 2. For any reservation that filled, launch the assigned task on the executor.
    /// 3. For any reservations that could not be filled, cancel the reservation (i.e. return the
    ///    task slot back to the pool of available task slots).
    ///
    /// NOTE Error handling in this method is very important. No matter what we need to ensure
    /// that unfilled reservations are cancelled or else they could become permanently "invisible"
    /// to the scheduler.
    pub(crate) async fn offer_reservation(
        &self,
        reservations: Vec<ExecutorReservation>,
    ) -> Result<Vec<ExecutorReservation>> {
        let (free_list, pending_tasks) = match self
            .task_manager
            .fill_reservations(&reservations)
            .await
        {
            Ok((assignments, mut unassigned_reservations, pending_tasks)) => {
                for (executor_id, task) in assignments.into_iter() {
                    match self
                        .executor_manager
                        .get_executor_metadata(&executor_id)
                        .await
                    {
                        Ok(executor) => {
                            if let Err(e) = self
                                .task_manager
                                .launch_task(&executor, task, &self.executor_manager)
                                .await
                            {
                                error!("Failed to launch new task: {:?}", e);
                                unassigned_reservations.push(
                                    ExecutorReservation::new_free(executor_id.clone()),
                                );
                            }
                        }
                        Err(e) => {
                            error!("Failed to launch new task, could not get executor metadata: {:?}", e);
                            unassigned_reservations
                                .push(ExecutorReservation::new_free(executor_id.clone()));
                        }
                    }
                }
                (unassigned_reservations, pending_tasks)
            }
            Err(e) => {
                error!("Error filling reservations: {:?}", e);
                (reservations, 0)
            }
        };

        dbg!(free_list.clone());
        dbg!(pending_tasks);

        let mut new_reservations = vec![];
        if !free_list.is_empty() {
            // If any reserved slots remain, return them to the pool
            self.executor_manager.cancel_reservations(free_list).await?;
        } else if pending_tasks > 0 {
            // If there are pending tasks available, try and schedule them
            let pending_reservations = self
                .executor_manager
                .reserve_slots(pending_tasks as u32)
                .await?;
            new_reservations.extend(pending_reservations);
        }

        Ok(new_reservations)
    }

    pub(crate) async fn submit_job(
        &self,
        job_id: &str,
        session_ctx: Arc<SessionContext>,
        plan: &LogicalPlan,
    ) -> Result<()> {
        let start = Instant::now();
        let optimized_plan = session_ctx.optimize(plan)?;

        println!("Calculated optimized plan: {:?}", optimized_plan);

        let plan = session_ctx.create_physical_plan(&optimized_plan).await?;

        self.task_manager
            .submit_job(job_id, &session_ctx.session_id(), plan)
            .await?;

        let elapsed = start.elapsed();

        info!("Planned job {} in {:?}", job_id, elapsed);

        Ok(())
    }
}

pub async fn with_lock<Out, F: Future<Output = Out>>(lock: Box<dyn Lock>, op: F) -> Out {
    let mut lock = lock;
    let result = op.await;
    lock.unlock().await;

    result
}

#[cfg(test)]
mod test {
    use crate::state::backend::standalone::StandaloneClient;
    use crate::state::SchedulerState;
    use ballista_core::config::{BallistaConfig, BALLISTA_DEFAULT_SHUFFLE_PARTITIONS};
    use ballista_core::error::Result;
    use ballista_core::serde::protobuf::{
        task_status, CompletedTask, PartitionId, PhysicalPlanNode, ShuffleWritePartition,
        TaskStatus,
    };
    use ballista_core::serde::scheduler::{
        ExecutorData, ExecutorMetadata, ExecutorSpecification,
    };
    use ballista_core::serde::BallistaCodec;
    use datafusion::arrow::datatypes::{DataType, Field, Schema};
    use datafusion::execution::context::default_session_builder;
    use datafusion::logical_expr::{col, sum};
    use datafusion::physical_plan::ExecutionPlan;
    use datafusion::prelude::SessionContext;
    use datafusion::test_util::scan_empty;
    use datafusion_proto::protobuf::LogicalPlanNode;
    use std::sync::Arc;

    // We should free any reservations which are not assigned
    #[tokio::test]
    async fn test_offer_free_reservations() -> Result<()> {
        let state_storage = Arc::new(StandaloneClient::try_new_temporary()?);
        let state: Arc<SchedulerState<LogicalPlanNode, PhysicalPlanNode>> =
            Arc::new(SchedulerState::new_with_default_scheduler_name(
                state_storage,
                default_session_builder,
                BallistaCodec::default(),
            ));

        let executors = test_executors(1, 4);

        let (executor_metadata, executor_data) = executors[0].clone();

        let reservations = state
            .executor_manager
            .register_executor(executor_metadata, executor_data, true)
            .await?;

        let result = state.offer_reservation(reservations).await?;

        assert!(result.is_empty());

        // All reservations should have been cancelled so we should be able to reserve them now
        let reservations = state.executor_manager.reserve_slots(4).await?;

        assert_eq!(reservations.len(), 4);

        Ok(())
    }

    // We should fill unbound reservations to any available task
    #[tokio::test]
    async fn test_offer_fill_reservations() -> Result<()> {
        let config = BallistaConfig::builder()
            .set(BALLISTA_DEFAULT_SHUFFLE_PARTITIONS, "4")
            .build()?;
        let state_storage = Arc::new(StandaloneClient::try_new_temporary()?);
        let state: Arc<SchedulerState<LogicalPlanNode, PhysicalPlanNode>> =
            Arc::new(SchedulerState::new_with_default_scheduler_name(
                state_storage,
                default_session_builder,
                BallistaCodec::default(),
            ));

        let session_ctx = state.session_manager.create_session(&config).await?;

        let plan = test_graph(session_ctx.clone()).await;

        // Create 4 jobs so we have four pending tasks
        state
            .task_manager
            .submit_job("job-1", session_ctx.session_id().as_str(), plan.clone())
            .await?;
        state
            .task_manager
            .submit_job("job-2", session_ctx.session_id().as_str(), plan.clone())
            .await?;
        state
            .task_manager
            .submit_job("job-3", session_ctx.session_id().as_str(), plan.clone())
            .await?;
        state
            .task_manager
            .submit_job("job-4", session_ctx.session_id().as_str(), plan.clone())
            .await?;

        let executors = test_executors(1, 4);

        let (executor_metadata, executor_data) = executors[0].clone();

        let reservations = state
            .executor_manager
            .register_executor(executor_metadata, executor_data, true)
            .await?;

        let result = state.offer_reservation(reservations).await?;

        assert!(result.is_empty());

        // All task slots should be assigned so we should not be able to reserve more tasks
        let reservations = state.executor_manager.reserve_slots(4).await?;

        assert_eq!(reservations.len(), 0);

        Ok(())
    }

    // We should generate a new event for tasks that are still pending
    #[tokio::test]
    async fn test_offer_resubmit_pending() -> Result<()> {
        let config = BallistaConfig::builder()
            .set(BALLISTA_DEFAULT_SHUFFLE_PARTITIONS, "4")
            .build()?;
        let state_storage = Arc::new(StandaloneClient::try_new_temporary()?);
        let state: Arc<SchedulerState<LogicalPlanNode, PhysicalPlanNode>> =
            Arc::new(SchedulerState::new_with_default_scheduler_name(
                state_storage,
                default_session_builder,
                BallistaCodec::default(),
            ));

        let session_ctx = state.session_manager.create_session(&config).await?;

        let plan = test_graph(session_ctx.clone()).await;

        // Create a job
        state
            .task_manager
            .submit_job("job-1", session_ctx.session_id().as_str(), plan.clone())
            .await?;

        let executors = test_executors(1, 4);

        let (executor_metadata, executor_data) = executors[0].clone();

        // Complete the first stage. So we should now have 4 pending tasks for this job stage 2
        let mut partitions: Vec<ShuffleWritePartition> = vec![];

        for partition_id in 0..4 {
            partitions.push(ShuffleWritePartition {
                partition_id: partition_id as u64,
                path: "some/path".to_string(),
                num_batches: 1,
                num_rows: 1,
                num_bytes: 1,
            })
        }

        state
            .task_manager
            .update_task_statuses(
                &executor_metadata,
                vec![TaskStatus {
                    task_id: Some(PartitionId {
                        job_id: "job-1".to_string(),
                        stage_id: 1,
                        partition_id: 0,
                    }),
                    metrics: vec![],
                    status: Some(task_status::Status::Completed(CompletedTask {
                        executor_id: "executor-1".to_string(),
                        partitions,
                    })),
                }],
            )
            .await?;

        state
            .executor_manager
            .register_executor(executor_metadata, executor_data, false)
            .await?;

        let reservations = state.executor_manager.reserve_slots(1).await?;

        assert_eq!(reservations.len(), 1);

        // Offer the reservation. It should be filled with one of the 4 pending tasks. The other 3 should
        // be reserved for the other 3 tasks, emitting another offer event
        let reservations = state.offer_reservation(reservations).await?;

        assert_eq!(reservations.len(), 3);

        // Remaining 3 task slots should be reserved for pending tasks
        let reservations = state.executor_manager.reserve_slots(4).await?;

        assert_eq!(reservations.len(), 0);

        Ok(())
    }

    fn test_executors(
        total_executors: usize,
        slots_per_executor: u32,
    ) -> Vec<(ExecutorMetadata, ExecutorData)> {
        let mut result: Vec<(ExecutorMetadata, ExecutorData)> = vec![];

        for i in 0..total_executors {
            result.push((
                ExecutorMetadata {
                    id: format!("executor-{}", i),
                    host: format!("host-{}", i),
                    port: 8080,
                    grpc_port: 9090,
                    specification: ExecutorSpecification {
                        task_slots: slots_per_executor,
                    },
                },
                ExecutorData {
                    executor_id: format!("executor-{}", i),
                    total_task_slots: slots_per_executor,
                    available_task_slots: slots_per_executor,
                },
            ));
        }

        result
    }

    async fn test_graph(ctx: Arc<SessionContext>) -> Arc<dyn ExecutionPlan> {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Utf8, false),
            Field::new("gmv", DataType::UInt64, false),
        ]);

        let plan = scan_empty(None, &schema, Some(vec![0, 1]))
            .unwrap()
            .aggregate(vec![col("id")], vec![sum(col("gmv"))])
            .unwrap()
            .build()
            .unwrap();

        ctx.create_physical_plan(&plan).await.unwrap()
    }
}

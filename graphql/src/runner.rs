use std::env;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::prelude::{QueryExecutionOptions, StoreResolver, SubscriptionExecutionOptions};
use crate::query::execute_query;
use crate::subscription::execute_prepared_subscription;
use graph::data::graphql::effort::LoadManager;
use graph::prelude::{
    async_trait, o, CheapClone, DeploymentState, GraphQlRunner as GraphQlRunnerTrait, Logger,
    Query, QueryExecutionError, QueryResult, Store, SubgraphDeploymentStore, Subscription,
    SubscriptionError, SubscriptionResult,
};

use lazy_static::lazy_static;

/// GraphQL runner implementation for The Graph.
pub struct GraphQlRunner<S> {
    logger: Logger,
    store: Arc<S>,
    load_manager: Arc<LoadManager>,
}

lazy_static! {
    static ref GRAPHQL_QUERY_TIMEOUT: Option<Duration> = env::var("GRAPH_GRAPHQL_QUERY_TIMEOUT")
        .ok()
        .map(|s| Duration::from_secs(
            u64::from_str(&s)
                .unwrap_or_else(|_| panic!("failed to parse env var GRAPH_GRAPHQL_QUERY_TIMEOUT"))
        ));
    static ref GRAPHQL_MAX_COMPLEXITY: Option<u64> = env::var("GRAPH_GRAPHQL_MAX_COMPLEXITY")
        .ok()
        .map(|s| u64::from_str(&s)
            .unwrap_or_else(|_| panic!("failed to parse env var GRAPH_GRAPHQL_MAX_COMPLEXITY")));
    static ref GRAPHQL_MAX_DEPTH: u8 = env::var("GRAPH_GRAPHQL_MAX_DEPTH")
        .ok()
        .map(|s| u8::from_str(&s)
            .unwrap_or_else(|_| panic!("failed to parse env var GRAPH_GRAPHQL_MAX_DEPTH")))
        .unwrap_or(u8::max_value());
    static ref GRAPHQL_MAX_FIRST: u32 = env::var("GRAPH_GRAPHQL_MAX_FIRST")
        .ok()
        .map(|s| u32::from_str(&s)
            .unwrap_or_else(|_| panic!("failed to parse env var GRAPH_GRAPHQL_MAX_FIRST")))
        .unwrap_or(1000);
    static ref GRAPHQL_MAX_SKIP: u32 = env::var("GRAPH_GRAPHQL_MAX_SKIP")
        .ok()
        .map(|s| u32::from_str(&s)
            .unwrap_or_else(|_| panic!("failed to parse env var GRAPH_GRAPHQL_MAX_SKIP")))
        .unwrap_or(std::u32::MAX);
    // Allow skipping the check whether a deployment has changed while
    // we were running a query. Once we are sure that the check mechanism
    // is reliable, this variable should be removed
    static ref GRAPHQL_ALLOW_DEPLOYMENT_CHANGE: bool = env::var("GRAPHQL_ALLOW_DEPLOYMENT_CHANGE")
        .ok()
        .map(|s| s == "true")
        .unwrap_or(false);
}

impl<S> GraphQlRunner<S>
where
    S: Store + SubgraphDeploymentStore,
{
    /// Creates a new query runner.
    pub fn new(logger: &Logger, store: Arc<S>, load_manager: Arc<LoadManager>) -> Self {
        let logger = logger.new(o!("component" => "GraphQlRunner"));
        GraphQlRunner {
            logger,
            store,
            load_manager,
        }
    }

    /// Check if the subgraph state differs from `state` now in a way that
    /// would affect a query that looked at data as fresh as `latest_block`.
    /// If the subgraph did change, return the `Err` that should be sent back
    /// to clients to indicate that condition
    fn deployment_changed(
        &self,
        state: DeploymentState,
        latest_block: u64,
    ) -> Result<(), QueryExecutionError> {
        if *GRAPHQL_ALLOW_DEPLOYMENT_CHANGE {
            return Ok(());
        }
        let new_state = self.store.deployment_state_from_id(state.id.clone())?;
        assert!(new_state.reorg_count >= state.reorg_count);
        if new_state.reorg_count > state.reorg_count {
            // One or more reorgs happened; each reorg can't have gone back
            // farther than `max_reorg_depth`, so that querying at blocks
            // far enough away from the previous latest block is fine. Taking
            // this into consideration is important, since most of the time
            // there is only one reorg of one block, and we therefore avoid
            // flagging a lot of queries a bit behind the head
            let n_blocks = new_state.max_reorg_depth * (new_state.reorg_count - state.reorg_count);
            if latest_block + n_blocks as u64 > state.latest_ethereum_block_number as u64 {
                return Err(QueryExecutionError::DeploymentReverted);
            }
        }
        Ok(())
    }

    async fn execute(
        &self,
        query: Query,
        state: DeploymentState,
        max_complexity: Option<u64>,
        max_depth: Option<u8>,
        max_first: Option<u32>,
        max_skip: Option<u32>,
        nested_resolver: bool,
    ) -> Result<Arc<QueryResult>, QueryResult> {
        // We need to use the same `QueryStore` for the entire query to ensure
        // we have a consistent view if the world, even when replicas, which
        // are eventually consistent, are in use. If we run different parts
        // of the query against different replicas, it would be possible for
        // them to be at wildly different states, and we might unwittingly
        // mix data from different block heights even if no reverts happen
        // while the query is running. `self.store` can not be used after this
        // point, and everything needs to go through the `store` we are
        // setting up here
        let store = self.store.cheap_clone().query_store(false);

        let max_depth = max_depth.unwrap_or(*GRAPHQL_MAX_DEPTH);
        let query = crate::execution::Query::new(&self.logger, query, max_complexity, max_depth)?;
        self.load_manager
            .decide(
                store.wait_stats(),
                query.shape_hash,
                query.query_text.as_ref(),
            )
            .to_result()?;

        let execute = |selection_set, resolver: StoreResolver| {
            execute_query(
                query.clone(),
                Some(selection_set),
                resolver.block_ptr.clone(),
                QueryExecutionOptions {
                    resolver,
                    deadline: GRAPHQL_QUERY_TIMEOUT.map(|t| Instant::now() + t),
                    max_first: max_first.unwrap_or(*GRAPHQL_MAX_FIRST),
                    max_skip: max_skip.unwrap_or(*GRAPHQL_MAX_SKIP),
                    load_manager: self.load_manager.clone(),
                },
                nested_resolver,
            )
        };
        // Unwrap: There is always at least one block constraint, even if it
        // is an implicit 'BlockContraint::Latest'.
        let mut by_block_constraint = query.block_constraint()?.into_iter();
        let (bc, selection_set) = by_block_constraint.next().unwrap();

        let resolver = StoreResolver::at_block(
            &self.logger,
            store.cheap_clone(),
            bc,
            query.schema.id().clone(),
        )
        .await?;
        let mut max_block = resolver.block_number();
        let mut result = execute(selection_set, resolver).await;

        // We want to optimize for the common case of a single block constraint, where we can avoid
        // cloning the result. If there are multiple constraints we have to clone.
        if by_block_constraint.len() > 0 {
            let mut partial_res = result.as_ref().clone();
            for (bc, selection_set) in by_block_constraint {
                let resolver = StoreResolver::at_block(
                    &self.logger,
                    store.cheap_clone(),
                    bc,
                    query.schema.id().clone(),
                )
                .await?;
                max_block = max_block.max(resolver.block_number());
                partial_res.append(execute(selection_set, resolver).await.as_ref().clone());
            }
            result = Arc::new(partial_res);
        }

        query.log_execution(max_block);
        self.deployment_changed(state, max_block as u64)
            .map_err(QueryResult::from)
            .map(|()| result)
    }
}

#[async_trait]
impl<S> GraphQlRunnerTrait for GraphQlRunner<S>
where
    S: Store + SubgraphDeploymentStore,
{
    async fn run_query(
        self: Arc<Self>,
        query: Query,
        state: DeploymentState,
        nested_resolver: bool,
    ) -> Arc<QueryResult> {
        self.run_query_with_complexity(
            query,
            state,
            *GRAPHQL_MAX_COMPLEXITY,
            Some(*GRAPHQL_MAX_DEPTH),
            Some(*GRAPHQL_MAX_FIRST),
            Some(*GRAPHQL_MAX_SKIP),
            nested_resolver,
        )
        .await
    }

    async fn run_query_with_complexity(
        self: Arc<Self>,
        query: Query,
        state: DeploymentState,
        max_complexity: Option<u64>,
        max_depth: Option<u8>,
        max_first: Option<u32>,
        max_skip: Option<u32>,
        nested_resolver: bool,
    ) -> Arc<QueryResult> {
        self.execute(
            query,
            state,
            max_complexity,
            max_depth,
            max_first,
            max_skip,
            nested_resolver,
        )
        .await
        .unwrap_or_else(|e| Arc::new(e))
    }

    async fn run_subscription(
        self: Arc<Self>,
        subscription: Subscription,
    ) -> Result<SubscriptionResult, SubscriptionError> {
        let query = crate::execution::Query::new(
            &self.logger,
            subscription.query,
            *GRAPHQL_MAX_COMPLEXITY,
            *GRAPHQL_MAX_DEPTH,
        )?;

        let store = self.store.clone().query_store(true);
        if let Err(err) = self
            .load_manager
            .decide(
                store.wait_stats(),
                query.shape_hash,
                query.query_text.as_ref(),
            )
            .to_result()
        {
            return Err(SubscriptionError::GraphQLError(vec![err]));
        }

        let deployment = query.schema.id().clone();
        execute_prepared_subscription(
            query,
            SubscriptionExecutionOptions {
                logger: self.logger.clone(),
                resolver: StoreResolver::for_subscription(&self.logger, deployment, store),
                timeout: GRAPHQL_QUERY_TIMEOUT.clone(),
                max_complexity: *GRAPHQL_MAX_COMPLEXITY,
                max_depth: *GRAPHQL_MAX_DEPTH,
                max_first: *GRAPHQL_MAX_FIRST,
                max_skip: *GRAPHQL_MAX_SKIP,
                load_manager: self.load_manager.cheap_clone(),
            },
        )
    }

    fn load_manager(&self) -> Arc<LoadManager> {
        self.load_manager.clone()
    }
}

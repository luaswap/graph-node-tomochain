use async_trait::async_trait;

use crate::prelude::*;

#[async_trait]
pub trait DataSourceLoader {
    async fn load_dynamic_data_sources(
        &self,
        id: SubgraphDeploymentId,
        logger: Logger,
    ) -> Result<Vec<DataSource>, Error>;
}

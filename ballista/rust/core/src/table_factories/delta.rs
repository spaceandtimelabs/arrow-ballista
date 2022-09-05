use std::collections::HashMap;
use std::sync::Arc;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::datasource::custom::CustomTable;
use datafusion::datasource::datasource::TableProviderFactory;
use datafusion::datasource::listing::{ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::datasource::TableProvider;
use datafusion::execution::context::SessionState;
use datafusion::prelude::ParquetReadOptions;
use async_trait::async_trait;
use datafusion::error::Result;

pub struct DeltaTableFactory {}

#[async_trait]
impl TableProviderFactory for DeltaTableFactory {
    async fn create(
        &self,
        _ctx: &SessionState,
        table_type: &str,
        url: &str,
        _options: HashMap<String, String>,
    ) -> Result<Arc<dyn TableProvider>> {
        let provider = deltalake::open_table(url)
            .await
            .unwrap();
        let table = CustomTable::new(table_type, url, HashMap::default(), Arc::new(provider));
        Ok(Arc::new(table))
    }

    fn with_schema(
        &self,
        _ctx: &SessionState,
        schema: SchemaRef,
        table_type: &str,
        url: &str,
        _options: HashMap<String, String>,
    ) -> Result<Arc<dyn TableProvider>> {
        let table_path = ListingTableUrl::parse(url)?;
        let partition_count = 1; // TODO: partitions
        let listing_options = ParquetReadOptions::default().to_listing_options(partition_count);
        let config = ListingTableConfig::new(table_path)
            .with_listing_options(listing_options)
            .with_schema(schema);

        let provider = Arc::new(ListingTable::try_new(config)?);

        let table = CustomTable::new(table_type, url, HashMap::default(), provider);
        Ok(Arc::new(table))
    }
}

use crate::ffi;
use crate::{IndexerError, IndexerResult, Manifest};
use fuel_indexer_database::{
    queries, types::IdCol, IndexerConnection, IndexerConnectionPool,
};
use fuel_indexer_schema::FtColumn;
use std::collections::HashMap;
use tracing::error;
use wasmer::Instance;

/// Database for an executor instance, with schema info.
#[derive(Debug)]
pub struct Database {
    pub pool: IndexerConnectionPool,
    stashed: Option<IndexerConnection>,
    pub namespace: String,
    pub identifier: String,
    pub version: String,
    pub schema: HashMap<String, Vec<String>>,
    pub tables: HashMap<i64, String>,
}

// TODO: Use mutex
unsafe impl Sync for Database {}
unsafe impl Send for Database {}

impl Database {
    pub async fn new(conn_uri: &str) -> IndexerResult<Database> {
        let pool = IndexerConnectionPool::connect(conn_uri).await?;

        Ok(Database {
            pool,
            stashed: None,
            namespace: Default::default(),
            identifier: Default::default(),
            version: Default::default(),
            schema: Default::default(),
            tables: Default::default(),
        })
    }

    pub async fn start_transaction(&mut self) -> IndexerResult<usize> {
        let mut conn = self.pool.acquire().await?;
        let result = queries::execute_query(&mut conn, "BEGIN".into()).await?;

        self.stashed = Some(conn);

        Ok(result)
    }

    pub async fn commit_transaction(&mut self) -> IndexerResult<usize> {
        let mut conn = self
            .stashed
            .take()
            .ok_or(IndexerError::NoTransactionError)?;
        Ok(queries::execute_query(&mut conn, "COMMIT".into()).await?)
    }

    pub async fn revert_transaction(&mut self) -> IndexerResult<usize> {
        let mut conn = self
            .stashed
            .take()
            .ok_or(IndexerError::NoTransactionError)?;
        Ok(queries::execute_query(&mut conn, "ROLLBACK".into()).await?)
    }

    fn upsert_query(
        &self,
        table: &str,
        columns: &[String],
        inserts: Vec<String>,
        updates: Vec<String>,
    ) -> String {
        format!(
            "INSERT INTO {}
                ({})
             VALUES
                ({}, $1)
             ON CONFLICT(id)
             DO UPDATE SET {}",
            table,
            columns.join(", "),
            inserts.join(", "),
            updates.join(", "),
        )
    }

    fn namespace(&self) -> String {
        format!("{}_{}", self.namespace, self.identifier)
    }

    fn get_query(&self, table: &str, object_id: u64) -> String {
        format!("SELECT object from {table} where id = {object_id}")
    }

    pub async fn put_object(
        &mut self,
        type_id: i64,
        columns: Vec<FtColumn>,
        bytes: Vec<u8>,
    ) {
        let table = match self.tables.get(&type_id) {
            Some(t) => t,
            None => {
                error!("TypeId({}) not found in tables: {:?}", type_id, self.tables,);
                return;
            }
        };

        let inserts: Vec<_> = columns.iter().map(|col| col.query_fragment()).collect();
        let updates: Vec<_> = self.schema[table]
            .iter()
            .zip(columns.iter())
            .filter_map(|(colname, value)| {
                if colname == &IdCol::to_lowercase_string() {
                    None
                } else {
                    Some(format!("{} = {}", colname, value.query_fragment()))
                }
            })
            .collect();

        let columns = self.schema[table].clone();

        let query_text = self.upsert_query(table, &columns, inserts, updates);

        let conn = self
            .stashed
            .as_mut()
            .expect("No transaction has been opened.");

        queries::put_object(conn, query_text, bytes)
            .await
            .expect("Failed to put object.");
    }

    pub async fn get_object(&mut self, type_id: i64, object_id: u64) -> Option<Vec<u8>> {
        let table = &self.tables[&type_id];
        let query = self.get_query(table, object_id);
        let conn = self
            .stashed
            .as_mut()
            .expect("No transaction has been opened.");

        match queries::get_object(conn, query).await {
            Ok(v) => Some(v),
            Err(_e) => None,
        }
    }

    pub async fn load_schema(
        &mut self,
        manifest: &Manifest,
        instance: Option<&Instance>,
    ) -> IndexerResult<()> {
        match manifest.is_native() {
            true => {
                self.namespace = manifest.namespace.clone();
                self.identifier = manifest.identifier.clone();

                let mut conn = self.pool.acquire().await?;
                self.version =
                    queries::type_id_latest(&mut conn, &self.namespace, &self.identifier)
                        .await?;

                let results = queries::columns_get_schema(
                    &mut conn,
                    &self.namespace,
                    &self.identifier,
                    &self.version,
                )
                .await?;

                for column in results {
                    let table = &format!("{}.{}", self.namespace(), &column.table_name);

                    self.tables
                        .entry(column.type_id)
                        .or_insert_with(|| table.to_string());

                    let columns = self
                        .schema
                        .entry(table.to_string())
                        .or_insert_with(Vec::new);

                    columns.push(column.column_name);
                }
            }
            false => {
                let instance = instance.unwrap();

                self.namespace = ffi::get_namespace(instance)?;
                self.identifier = ffi::get_identifier(instance)?;
                self.version = ffi::get_version(instance)?;

                let mut conn = self.pool.acquire().await?;
                let results = queries::columns_get_schema(
                    &mut conn,
                    &self.namespace,
                    &self.identifier,
                    &self.version,
                )
                .await?;

                for column in results {
                    let table = &format!("{}.{}", self.namespace(), &column.table_name);

                    self.tables
                        .entry(column.type_id)
                        .or_insert_with(|| table.to_string());

                    let columns = self
                        .schema
                        .entry(table.to_string())
                        .or_insert_with(Vec::new);

                    columns.push(column.column_name);
                }
            }
        }

        Ok(())
    }
}

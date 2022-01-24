mod bind;
mod raw;
mod stmt;
mod url;

use self::raw::RawConnection;
use self::stmt::iterator::StatementIterator;
use self::stmt::Statement;
use self::url::ConnectionOptions;
use super::backend::Mysql;
use crate::connection::commit_error_processor::{
    default_process_commit_error, CommitErrorOutcome, CommitErrorProcessor,
};
use crate::connection::*;
use crate::expression::QueryMetadata;
use crate::query_builder::bind_collector::RawBytesBindCollector;
use crate::query_builder::*;
use crate::result::*;

#[allow(missing_debug_implementations, missing_copy_implementations)]
/// A connection to a MySQL database. Connection URLs should be in the form
/// `mysql://[user[:password]@]host/database_name`
pub struct MysqlConnection {
    raw_connection: RawConnection,
    transaction_state: AnsiTransactionManager,
    statement_cache: StatementCache<Mysql, Statement>,
}

unsafe impl Send for MysqlConnection {}

impl SimpleConnection for MysqlConnection {
    fn batch_execute(&mut self, query: &str) -> QueryResult<()> {
        self.raw_connection
            .enable_multi_statements(|| self.raw_connection.execute(query))
    }
}

impl<'conn, 'query> ConnectionGatWorkaround<'conn, 'query, Mysql> for MysqlConnection {
    type Cursor = self::stmt::iterator::StatementIterator<'conn>;
    type Row = self::stmt::iterator::MysqlRow;
}

impl CommitErrorProcessor for MysqlConnection {
    fn process_commit_error(&self, error: Error) -> CommitErrorOutcome {
        let state = match self.transaction_state.status {
            TransactionManagerStatus::InError => {
                return CommitErrorOutcome::Throw(Error::BrokenTransaction)
            }
            TransactionManagerStatus::Valid(ref v) => v,
        };
        default_process_commit_error(state, error)
    }
}

impl Connection for MysqlConnection {
    type Backend = Mysql;
    type TransactionManager = AnsiTransactionManager;

    fn establish(database_url: &str) -> ConnectionResult<Self> {
        use crate::result::ConnectionError::CouldntSetupConfiguration;

        let raw_connection = RawConnection::new();
        let connection_options = ConnectionOptions::parse(database_url)?;
        raw_connection.connect(&connection_options)?;
        let mut conn = MysqlConnection {
            raw_connection,
            transaction_state: AnsiTransactionManager::default(),
            statement_cache: StatementCache::new(),
        };
        conn.set_config_options()
            .map_err(CouldntSetupConfiguration)?;
        Ok(conn)
    }

    #[doc(hidden)]
    fn execute(&mut self, query: &str) -> QueryResult<usize> {
        self.raw_connection
            .execute(query)
            .map(|_| self.raw_connection.affected_rows())
    }

    #[doc(hidden)]
    fn load<'conn, 'query, T>(
        &'conn mut self,
        source: T,
    ) -> QueryResult<<Self as ConnectionGatWorkaround<'conn, 'query, Self::Backend>>::Cursor>
    where
        T: AsQuery,
        T::Query: QueryFragment<Self::Backend> + QueryId + 'query,
        Self::Backend: QueryMetadata<T::SqlType>,
    {
        let stmt = self.prepared_query(&source.as_query())?;

        let mut metadata = Vec::new();
        Mysql::row_metadata(&mut (), &mut metadata);

        StatementIterator::from_stmt(stmt, &metadata)
    }

    #[doc(hidden)]
    fn execute_returning_count<T>(&mut self, source: &T) -> QueryResult<usize>
    where
        T: QueryFragment<Self::Backend> + QueryId,
    {
        let stmt = self.prepared_query(source)?;
        unsafe {
            stmt.execute()?;
        }
        Ok(stmt.affected_rows())
    }

    #[doc(hidden)]
    fn transaction_state(&mut self) -> &mut AnsiTransactionManager {
        &mut self.transaction_state
    }
}

#[cfg(feature = "r2d2")]
impl crate::r2d2::R2D2Connection for MysqlConnection {
    fn ping(&mut self) -> QueryResult<()> {
        self.execute("SELECT 1").map(|_| ())
    }

    fn is_broken(&mut self) -> bool {
        self.transaction_state
            .status
            .transaction_depth()
            .map(|d| d.is_none())
            .unwrap_or(true)
    }
}

impl MysqlConnection {
    fn prepared_query<'a, T: QueryFragment<Mysql> + QueryId>(
        &'a mut self,
        source: &'_ T,
    ) -> QueryResult<MaybeCached<'a, Statement>> {
        let cache = &mut self.statement_cache;
        let conn = &mut self.raw_connection;

        let mut stmt = cache.cached_statement(source, &[], |sql, _| conn.prepare(sql))?;
        let mut bind_collector = RawBytesBindCollector::new();
        source.collect_binds(&mut bind_collector, &mut ())?;
        let binds = bind_collector
            .metadata
            .into_iter()
            .zip(bind_collector.binds);
        stmt.bind(binds)?;
        Ok(stmt)
    }

    fn set_config_options(&mut self) -> QueryResult<()> {
        self.execute("SET sql_mode=(SELECT CONCAT(@@sql_mode, ',PIPES_AS_CONCAT'))")?;
        self.execute("SET time_zone = '+00:00';")?;
        self.execute("SET character_set_client = 'utf8mb4'")?;
        self.execute("SET character_set_connection = 'utf8mb4'")?;
        self.execute("SET character_set_results = 'utf8mb4'")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    extern crate dotenv;

    use super::*;
    use std::env;

    fn connection() -> MysqlConnection {
        dotenv::dotenv().ok();
        let database_url = env::var("MYSQL_UNIT_TEST_DATABASE_URL")
            .or_else(|_| env::var("MYSQL_DATABASE_URL"))
            .or_else(|_| env::var("DATABASE_URL"))
            .expect("DATABASE_URL must be set in order to run unit tests");
        MysqlConnection::establish(&database_url).unwrap()
    }

    #[test]
    fn batch_execute_handles_single_queries_with_results() {
        let connection = &mut connection();
        assert!(connection.batch_execute("SELECT 1").is_ok());
        assert!(connection.batch_execute("SELECT 1").is_ok());
    }

    #[test]
    fn batch_execute_handles_multi_queries_with_results() {
        let connection = &mut connection();
        let query = "SELECT 1; SELECT 2; SELECT 3;";
        assert!(connection.batch_execute(query).is_ok());
        assert!(connection.batch_execute(query).is_ok());
    }

    #[test]
    fn execute_handles_queries_which_return_results() {
        let connection = &mut connection();
        assert!(connection.execute("SELECT 1").is_ok());
        assert!(connection.execute("SELECT 1").is_ok());
    }

    #[test]
    fn check_client_found_rows_flag() {
        let conn = &mut crate::test_helpers::connection();
        conn.execute("DROP TABLE IF EXISTS update_test CASCADE")
            .unwrap();

        conn.execute("CREATE TABLE update_test(id INTEGER PRIMARY KEY, num INTEGER NOT NULL)")
            .unwrap();

        conn.execute("INSERT INTO update_test(id, num) VALUES (1, 5)")
            .unwrap();

        let output = conn
            .execute("UPDATE update_test SET num = 5 WHERE id = 1")
            .unwrap();

        assert_eq!(output, 1);
    }
}

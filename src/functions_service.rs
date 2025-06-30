use log::warn;
use std::str::FromStr;
use uuid::Uuid;

#[derive(Clone)]
pub struct FunctionSupabaseService {
    client_pool: deadpool_postgres::Pool,
}

impl FunctionSupabaseService {
    pub fn from_env() -> Self {
        let pg_config = tokio_postgres::Config::from_str(&crate::env::POSTGRES_URL)
            .expect("Invalid POSTGRES_URL variable");

        let mgr_config = deadpool_postgres::ManagerConfig {
            recycling_method: deadpool_postgres::RecyclingMethod::Fast,
        };

        let mgr =
            deadpool_postgres::Manager::from_config(pg_config, tokio_postgres::NoTls, mgr_config);

        let client_pool = deadpool_postgres::Pool::builder(mgr)
            .max_size(16)
            .build()
            .unwrap();

        FunctionSupabaseService { client_pool }
    }

    pub async fn check_connection(&self) -> Result<(), deadpool_postgres::PoolError> {
        // Just to ensure the connection is established
        self.client_pool.get().await.map(|_| ())
    }
}

impl FunctionService for FunctionSupabaseService {
    async fn get_last_depl_time(
        &self,
        function_uuid: &Uuid,
    ) -> Result<Option<u64>, tokio_postgres::Error> {
        let client = self.client_pool.get().await.unwrap();
        let stmt = client
            .prepare_cached(
                "select round(extract(epoch from fd.created_at))::bigint as \"last_deploy_creation_date\"
    from function_deployments fd
    where
       fd.function_id = $1
       and fd.status = 'success'
    order by fd.created_at desc
    limit 1;",
            )
            .await?;

        let mut result = client.query(&stmt, &[&function_uuid]).await?;

        if result.is_empty() {
            warn!(
                "No successful deployments found for function_id={function_uuid:?}. Returning 404.",
            );
            return Ok(None);
        }

        let result = result.pop().unwrap();
        let last_deploy_creation_date: i64 = result.get("last_deploy_creation_date");

        if last_deploy_creation_date < 0 {
            warn!(
                "Last deployment creation date is negative: {} for function_id: {}. Using 0",
                last_deploy_creation_date, function_uuid
            );
            Ok(Some(0))
        } else {
            Ok(Some(last_deploy_creation_date as u64))
        }
    }
}

pub trait FunctionService {
    async fn get_last_depl_time(
        &self,
        function_id: &Uuid,
    ) -> Result<Option<u64>, tokio_postgres::Error>;
}

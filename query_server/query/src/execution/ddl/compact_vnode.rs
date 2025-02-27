use async_trait::async_trait;

use spi::query::execution::{Output, QueryStateMachineRef};

use super::DDLDefinitionTask;
use meta::error::MetaError;

use spi::query::logical_planner::CompactVnode;
use spi::QueryError;
use spi::Result;

pub struct CompactVnodeTask {
    stmt: CompactVnode,
}

impl CompactVnodeTask {
    #[inline(always)]
    pub fn new(stmt: CompactVnode) -> Self {
        Self { stmt }
    }
}

#[async_trait]
impl DDLDefinitionTask for CompactVnodeTask {
    async fn execute(&self, query_state_machine: QueryStateMachineRef) -> Result<Output> {
        let CompactVnode { vnode_ids: _ } = self.stmt;
        let tenant = query_state_machine.session.tenant();
        let _meta = query_state_machine
            .meta
            .tenant_manager()
            .tenant_meta(tenant)
            .ok_or_else(|| QueryError::Meta {
                source: MetaError::TenantNotFound {
                    tenant: tenant.to_string(),
                },
            })?;
        todo!()
    }
}

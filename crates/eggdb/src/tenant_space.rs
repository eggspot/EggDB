use crate::scan::Scan;
use crate::{Database, Error, ErrorKind, Result};
use eggdb_cluster::tenants::{split_tenant_key, tenant_key, tenant_prefix};
use std::ops::Bound;

/// One tenant's key space (D1047): its keys are stored under the tenant prefix of the key
/// layout, so they form one range that moves between shards as a unit and that the default key
/// space never shows.
#[derive(Debug, Clone, Copy)]
pub struct TenantSpace<'d> {
    database: &'d Database,
    tenant: &'d str,
}

fn bad_tenant(tenant: &str) -> Error {
    Error::new(
        ErrorKind::Other,
        format!("a tenant's name is 1 to 255 bytes, not {} (\"{tenant}\")", tenant.len()),
    )
}

fn after_prefix(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xFF {
            end.push(last + 1);
            return end;
        }
    }
    vec![0xFF; prefix.len() + 1]
}

impl Database {
    /// The key space of `tenant` (D1047).
    pub fn tenant<'d>(&'d self, tenant: &'d str) -> Result<TenantSpace<'d>> {
        tenant_prefix(tenant).ok_or_else(|| bad_tenant(tenant))?;
        Ok(TenantSpace { database: self, tenant })
    }

    /// Up to `limit` of `tenant`'s stored keys after `after` (a stored key), with their values,
    /// from one snapshot, and the last key returned to continue after: a tenant move copies
    /// its range page by page while the tenant keeps writing.
    pub fn tenant_page(&self, tenant: &str, after: Option<&[u8]>, limit: usize) -> Result<(Vec<(Vec<u8>, Vec<u8>)>, Option<Vec<u8>>)> {
        let prefix = tenant_prefix(tenant).ok_or_else(|| bad_tenant(tenant))?;
        let read = self.begin_read()?;
        let start = match after {
            Some(after) => Bound::Excluded(after.to_vec()),
            None => Bound::Included(prefix.clone()),
        };
        let range = (start, Bound::Excluded(after_prefix(&prefix)));
        let mut entries = Vec::new();
        for item in Scan::within_between(read.reader(), read.tree(), b"", &range) {
            entries.push(item?);
            if entries.len() >= limit.max(1) {
                let next = entries.last().map(|(key, _): &(Vec<u8>, Vec<u8>)| key.clone());
                return Ok((entries, next));
            }
        }
        Ok((entries, None))
    }
}

impl TenantSpace<'_> {
    fn stored(&self, key: &[u8]) -> Result<Vec<u8>> {
        tenant_key(self.tenant, key).ok_or_else(|| bad_tenant(self.tenant))
    }

    /// The tenant's name.
    pub fn name(&self) -> &str {
        self.tenant
    }

    /// The value of `key` in this tenant's space.
    pub fn get(&self, key: impl AsRef<[u8]>) -> Result<Option<Vec<u8>>> {
        let stored = self.stored(key.as_ref())?;
        self.database.begin_read()?.get_raw(&stored)
    }

    /// Stores `value` under `key` in this tenant's space, in one write transaction.
    pub fn put(&self, key: impl AsRef<[u8]>, value: impl AsRef<[u8]>) -> Result<()> {
        let stored = self.stored(key.as_ref())?;
        let mut transaction = self.database.begin_write()?;
        transaction.put_raw(&stored, value.as_ref())?;
        transaction.commit()?;
        Ok(())
    }

    /// Removes `key` from this tenant's space; returns whether it was there.
    pub fn delete(&self, key: impl AsRef<[u8]>) -> Result<bool> {
        let stored = self.stored(key.as_ref())?;
        let mut transaction = self.database.begin_write()?;
        let removed = transaction.delete_raw(&stored)?;
        transaction.commit()?;
        Ok(removed)
    }

    /// Every key of this tenant's space with its value, in key order.
    pub fn entries(&self) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut after: Option<Vec<u8>> = None;
        loop {
            let (page, next) = self.database.tenant_page(self.tenant, after.as_deref(), 1024)?;
            for (stored, value) in page {
                if let Some((_, key)) = split_tenant_key(&stored) {
                    out.push((key.to_vec(), value));
                }
            }
            match next {
                Some(next) => after = Some(next),
                None => return Ok(out),
            }
        }
    }
}

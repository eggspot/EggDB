use eggdb::Database;
use eggdb::sql::{OwnedStore, SqlSnapshot, SqlWriter, Store as DatabaseStore};
use eggdb_sql::{
    DatabaseDirectory, KeyValueStore, ServedDatabaseError, ServedDatabases, SqlResult,
    is_valid_database_name,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

const DATABASE_EXTENSION: &str = "eggdb";

pub(super) struct ServedDirectory<'d> {
    default_name: String,
    default: &'d DatabaseStore<'d>,
    directory: Option<PathBuf>,
    options: eggdb::Options,
    read_only: bool,
    others: Mutex<BTreeMap<String, Arc<OwnedStore>>>,
    listing_version: AtomicU64,
    tenants: Option<TenantFiles>,
}

/// Databases per tenant (D1113): kept under `<directory>/tenants/<aa>/<bb>/<name>.eggdb`, the
/// two levels from the name's SHA-256 so no directory holds more than a few thousand files at
/// millions of tenants, opened when first used, and closed again once `max_open` are open and
/// they are the least recently used ones no session holds. Tenants suspended or deleted through
/// the tenant API are refused.
pub(super) struct TenantFiles {
    root: PathBuf,
    max_open: usize,
    used: Mutex<BTreeMap<String, u64>>,
    tick: AtomicU64,
    refused: Mutex<BTreeSet<String>>,
}

impl TenantFiles {
    /// Tenant files under `directory/tenants`, at most `max_open` open at once, refusing the
    /// tenants in `refused`.
    pub(super) fn new(directory: &std::path::Path, max_open: usize, refused: BTreeSet<String>) -> Self {
        Self {
            root: directory.join("tenants"),
            max_open: max_open.max(1),
            used: Mutex::new(BTreeMap::new()),
            tick: AtomicU64::new(0),
            refused: Mutex::new(refused),
        }
    }

    /// Where tenant `name`'s file is.
    pub(super) fn path_of(&self, name: &str) -> PathBuf {
        let digest = eggdb_crypto::sha256(name.as_bytes());
        self.root
            .join(format!("{:02x}", digest[0]))
            .join(format!("{:02x}", digest[1]))
            .join(format!("{name}.{DATABASE_EXTENSION}"))
    }

    /// Refuses tenant `name` from now on, or serves it again.
    pub(super) fn set_refused(&self, name: &str, refused: bool) {
        let mut set = self.refused.lock().unwrap_or_else(PoisonError::into_inner);
        if refused {
            set.insert(name.to_owned());
        } else {
            set.remove(name);
        }
    }

    fn is_refused(&self, name: &str) -> bool {
        self.refused.lock().unwrap_or_else(PoisonError::into_inner).contains(name)
    }

    fn touch(&self, name: &str) {
        let tick = self.tick.fetch_add(1, Ordering::Relaxed);
        self.used.lock().unwrap_or_else(PoisonError::into_inner).insert(name.to_owned(), tick);
    }
}

pub(super) enum AttachedDatabase<'a> {
    Default(&'a DatabaseStore<'a>),
    Other(Arc<OwnedStore>),
}

pub(super) struct ServedDirectorySetup<'d> {
    pub(super) default_name: &'d str,
    pub(super) default: &'d DatabaseStore<'d>,
    pub(super) directory: Option<PathBuf>,
    pub(super) options: eggdb::Options,
    pub(super) read_only: bool,
    pub(super) others: Vec<(String, Database)>,
    pub(super) tenants: Option<TenantFiles>,
}

impl<'d> ServedDirectory<'d> {
    pub(super) fn new(setup: ServedDirectorySetup<'d>) -> Self {
        let others = setup
            .others
            .into_iter()
            .map(|(name, db)| (name, Arc::new(OwnedStore(db))))
            .collect();
        Self {
            default_name: setup.default_name.to_owned(),
            default: setup.default,
            directory: setup.directory,
            options: setup.options,
            read_only: setup.read_only,
            others: Mutex::new(others),
            listing_version: AtomicU64::new(0),
            tenants: setup.tenants,
        }
    }

    pub(super) fn with_databases<R>(&self, answer: impl FnOnce(&[(&str, &Database)]) -> R) -> R {
        let others = self.others();
        let mut databases: Vec<(&str, &Database)> = Vec::with_capacity(others.len() + 1);
        databases.push((self.default_name.as_str(), self.default.0));
        databases.extend(others.iter().map(|(name, store)| (name.as_str(), &store.0)));
        answer(&databases)
    }

    pub(super) fn into_databases(self) -> Vec<Database> {
        let others = self
            .others
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner);
        others
            .into_values()
            .filter_map(|store| Arc::try_unwrap(store).ok())
            .map(|store| store.0)
            .collect()
    }

    fn others(&self) -> MutexGuard<'_, BTreeMap<String, Arc<OwnedStore>>> {
        self.others.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn changing_directory(&self) -> Result<&PathBuf, ServedDatabaseError> {
        let Some(directory) = &self.directory else {
            return Err(ServedDatabaseError::Unsupported(
                "eggdb serve <file> serves that one database; serve a directory (eggdb serve <directory>), which holds one .eggdb file per database, to create and drop databases"
                    .to_owned(),
            ));
        };
        if self.read_only {
            return Err(ServedDatabaseError::Unsupported(
                "eggdb serve --read-only neither creates nor drops databases".to_owned(),
            ));
        }
        Ok(directory)
    }

    /// The tenant files, when this server keeps a database per tenant.
    pub(super) fn tenant_files(&self) -> Option<&TenantFiles> {
        self.tenants.as_ref()
    }

    /// Closes tenant databases past the open limit, the least recently used first, skipping
    /// those a session holds.
    fn close_idle_tenants(&self, others: &mut BTreeMap<String, Arc<OwnedStore>>) {
        let Some(tenants) = &self.tenants else {
            return;
        };
        let used = tenants.used.lock().unwrap_or_else(PoisonError::into_inner).clone();
        let mut open: Vec<(u64, String)> = others
            .keys()
            .filter_map(|name| used.get(name).map(|tick| (*tick, name.clone())))
            .collect();
        if open.len() <= tenants.max_open {
            return;
        }
        open.sort();
        let mut excess = open.len() - tenants.max_open;
        for (_, name) in open {
            if excess == 0 {
                break;
            }
            if others.get(&name).is_some_and(|store| Arc::strong_count(store) == 1) {
                others.remove(&name);
                tenants.used.lock().unwrap_or_else(PoisonError::into_inner).remove(&name);
                excess -= 1;
            }
        }
    }

    /// Opens tenant `name`'s database when its file exists and it is not refused.
    fn open_tenant(&self, name: &str) -> Option<Arc<OwnedStore>> {
        let tenants = self.tenants.as_ref()?;
        if !is_valid_database_name(name) || tenants.is_refused(name) {
            return None;
        }
        let path = tenants.path_of(name);
        if !path.is_file() {
            return None;
        }
        let options = self.options.clone().create_if_missing(false).read_only(self.read_only);
        let db = Database::open_with(&path, options).ok()?;
        let store = Arc::new(OwnedStore(db));
        let mut others = self.others();
        let store = Arc::clone(others.entry(name.to_owned()).or_insert(store));
        tenants.touch(name);
        self.close_idle_tenants(&mut others);
        Some(store)
    }

    fn changed_listing(&self) {
        self.listing_version.fetch_add(1, Ordering::AcqRel);
    }

    /// The database `name` of the directory (not the default one), to move it elsewhere.
    pub(super) fn database_handle(&self, name: &str) -> Option<Arc<OwnedStore>> {
        let others = self.others();
        served_key(&others, name).and_then(|key| others.get(key)).cloned()
    }

    /// Where a database moved in from another server goes before it is served.
    pub(super) fn arrival_path(&self, name: &str) -> Result<PathBuf, ServedDatabaseError> {
        let directory = self.changing_directory()?;
        if !is_valid_database_name(name) {
            return Err(ServedDatabaseError::Failed(format!("\"{name}\" cannot name a database file")));
        }
        if name.eq_ignore_ascii_case(&self.default_name) || served_key(&self.others(), name).is_some() {
            return Err(ServedDatabaseError::Exists);
        }
        let path = directory.join(format!("{name}.{DATABASE_EXTENSION}"));
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(ServedDatabaseError::Exists);
        }
        Ok(path)
    }

    /// Starts serving the database file at `path`, which a move just put in place, as `name`.
    pub(super) fn adopt(&self, name: &str, path: &std::path::Path) -> Result<(), ServedDatabaseError> {
        let options = self.options.clone().create_if_missing(false).read_only(false);
        let db = Database::open_with(path, options)
            .map_err(|error| ServedDatabaseError::Failed(format!("cannot open {}: {error}", path.display())))?;
        self.others().insert(name.to_owned(), Arc::new(OwnedStore(db)));
        self.changed_listing();
        Ok(())
    }
}

fn served_key<'m>(others: &'m BTreeMap<String, Arc<OwnedStore>>, name: &str) -> Option<&'m str> {
    if let Some((key, _)) = others.get_key_value(name) {
        return Some(key.as_str());
    }
    others
        .keys()
        .find(|key| key.eq_ignore_ascii_case(name))
        .map(String::as_str)
}

impl ServedDatabases for ServedDirectory<'_> {
    fn names(&self) -> Vec<String> {
        let others = self.others();
        let mut names = Vec::with_capacity(others.len() + 1);
        names.push(self.default_name.clone());
        names.extend(others.keys().cloned());
        names
    }

    fn create(&self, name: &str) -> Result<(), ServedDatabaseError> {
        let directory = self.changing_directory()?;
        if !is_valid_database_name(name) {
            return Err(ServedDatabaseError::Failed(format!(
                "\"{name}\" cannot name a database file"
            )));
        }
        let mut others = self.others();
        if name.eq_ignore_ascii_case(&self.default_name) || served_key(&others, name).is_some() {
            return Err(ServedDatabaseError::Exists);
        }
        let path = match &self.tenants {
            Some(tenants) => tenants.path_of(name),
            None => directory.join(format!("{name}.{DATABASE_EXTENSION}")),
        };
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(ServedDatabaseError::Exists);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                ServedDatabaseError::Failed(format!("cannot create {}: {error}", parent.display()))
            })?;
        }
        let options = self
            .options
            .clone()
            .create_if_missing(true)
            .read_only(false);
        let db = Database::open_with(&path, options).map_err(|error| {
            ServedDatabaseError::Failed(format!("cannot create {}: {error}", path.display()))
        })?;
        others.insert(name.to_owned(), Arc::new(OwnedStore(db)));
        if let Some(tenants) = &self.tenants {
            tenants.touch(name);
            self.close_idle_tenants(&mut others);
        }
        self.changed_listing();
        Ok(())
    }

    fn drop_database(&self, name: &str) -> Result<(), ServedDatabaseError> {
        self.changing_directory()?;
        if name.eq_ignore_ascii_case(&self.default_name) {
            return Err(ServedDatabaseError::InUse);
        }
        if served_key(&self.others(), name).is_none() && self.open_tenant(name).is_none() {
            return Err(ServedDatabaseError::Missing);
        }
        let mut others = self.others();
        let Some(key) = served_key(&others, name).map(str::to_owned) else {
            return Err(ServedDatabaseError::Missing);
        };
        let Some(store) = others.remove(&key) else {
            return Err(ServedDatabaseError::Missing);
        };
        let store = match Arc::try_unwrap(store) {
            Ok(store) => store,
            Err(shared) => {
                others.insert(key, shared);
                return Err(ServedDatabaseError::InUse);
            }
        };
        self.changed_listing();
        let path = store.0.path().to_path_buf();
        let closed = store.close();
        std::fs::remove_file(&path).map_err(|error| {
            ServedDatabaseError::Failed(format!("cannot remove {}: {error}", path.display()))
        })?;
        closed.map_err(|error| {
            ServedDatabaseError::Failed(format!(
                "{} was removed, but closing it first failed: {error}",
                path.display()
            ))
        })
    }

    fn listing_version(&self) -> Option<u64> {
        Some(self.listing_version.load(Ordering::Acquire))
    }

    fn is_superuser_in(&self, name: &str, role: &str) -> bool {
        self.stored_role_in(name, role)
            .is_some_and(|stored| stored.login && stored.superuser)
    }
}

impl ServedDirectory<'_> {
    fn stored_role_in(&self, name: &str, role: &str) -> Option<eggdb_sql::StoredRole> {
        let database = if name.eq_ignore_ascii_case(&self.default_name) {
            AttachedDatabase::Default(self.default)
        } else {
            let others = self.others();
            let store = served_key(&others, name).and_then(|key| others.get(key))?;
            AttachedDatabase::Other(Arc::clone(store))
        };
        database
            .begin_read()
            .ok()
            .and_then(|reader| eggdb_sql::stored_role(&reader, role).ok().flatten())
    }
}

impl DatabaseDirectory for ServedDirectory<'_> {
    type Attached<'a>
        = AttachedDatabase<'a>
    where
        Self: 'a;

    fn attach(&self, name: &str) -> Option<AttachedDatabase<'_>> {
        if name == self.default_name {
            return Some(AttachedDatabase::Default(self.default));
        }
        if let Some(tenants) = &self.tenants
            && tenants.is_refused(name)
        {
            return None;
        }
        let open = self.others().get(name).map(Arc::clone);
        match open {
            Some(store) => {
                if let Some(tenants) = &self.tenants {
                    tenants.touch(name);
                }
                Some(AttachedDatabase::Other(store))
            }
            None => self.open_tenant(name).map(AttachedDatabase::Other),
        }
    }
}

impl KeyValueStore for AttachedDatabase<'_> {
    type Read<'s>
        = SqlSnapshot<'s>
    where
        Self: 's;
    type Write<'s>
        = SqlWriter<'s>
    where
        Self: 's;

    fn begin_read(&self) -> SqlResult<SqlSnapshot<'_>> {
        match self {
            Self::Default(store) => store.begin_read(),
            Self::Other(store) => store.begin_read(),
        }
    }

    fn begin_write(&self) -> SqlResult<SqlWriter<'_>> {
        match self {
            Self::Default(store) => store.begin_write(),
            Self::Other(store) => store.begin_write(),
        }
    }

    fn page_lookups(&self) -> Option<eggdb_sql::PageLookups> {
        match self {
            Self::Default(store) => store.page_lookups(),
            Self::Other(store) => store.page_lookups(),
        }
    }

    fn store_id(&self) -> usize {
        match self {
            Self::Default(store) => store.store_id(),
            Self::Other(store) => KeyValueStore::store_id(&**store),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "eggdb-served-directory-{label}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn serving<'d>(
        scratch: &Scratch,
        default: &'d DatabaseStore<'d>,
        directory: bool,
    ) -> ServedDirectory<'d> {
        ServedDirectory::new(ServedDirectorySetup {
            default_name: "main",
            default,
            directory: directory.then(|| scratch.0.clone()),
            options: eggdb::Options::new(),
            read_only: false,
            others: Vec::new(),
            tenants: None,
        })
    }

    #[test]
    fn tenant_databases_open_on_first_use_and_close_past_the_cap() {
        let scratch = Scratch::new("tenants");
        let db = Database::open(scratch.0.join("main.eggdb")).expect("main");
        let default = DatabaseStore(&db);
        let served = ServedDirectory::new(ServedDirectorySetup {
            default_name: "main",
            default: &default,
            directory: Some(scratch.0.clone()),
            options: eggdb::Options::new(),
            read_only: false,
            others: Vec::new(),
            tenants: Some(TenantFiles::new(&scratch.0, 1, BTreeSet::new())),
        });
        served.create("alice").expect("alice");
        served.create("bob").expect("bob");
        let tenants = served.tenant_files().expect("tenant files");
        assert!(tenants.path_of("alice").is_file());
        assert!(tenants.path_of("alice").starts_with(scratch.0.join("tenants")));
        assert_eq!(served.others().len(), 1);
        assert!(served.attach("alice").is_some());
        assert!(served.others().contains_key("alice"));
        assert!(!served.others().contains_key("bob"));
        tenants.set_refused("bob", true);
        assert!(served.attach("bob").is_none());
        tenants.set_refused("bob", false);
        assert!(served.attach("bob").is_some());
        assert!(served.attach("carol").is_none());
    }

    #[test]
    fn creating_a_database_makes_its_file_and_lists_it() {
        let scratch = Scratch::new("create");
        let db = Database::open(scratch.0.join("main.eggdb")).expect("main");
        let store = DatabaseStore(&db);
        let served = serving(&scratch, &store, true);
        let before = served.listing_version();
        served.create("shop").expect("create");
        assert!(scratch.0.join("shop.eggdb").is_file());
        assert_eq!(served.names(), vec!["main".to_owned(), "shop".to_owned()]);
        assert_ne!(served.listing_version(), before);
        assert_eq!(served.create("SHOP"), Err(ServedDatabaseError::Exists));
        assert_eq!(served.create("main"), Err(ServedDatabaseError::Exists));
        assert_eq!(served.matching_name("Shop"), Some("shop".to_owned()));
        drop(served.into_databases());
    }

    #[test]
    fn a_database_in_use_is_not_dropped_until_its_connection_lets_go() {
        let scratch = Scratch::new("drop");
        let db = Database::open(scratch.0.join("main.eggdb")).expect("main");
        let store = DatabaseStore(&db);
        let served = serving(&scratch, &store, true);
        served.create("shop").expect("create");
        let attached = served.attach("shop").expect("attached");
        assert_eq!(
            served.drop_database("shop"),
            Err(ServedDatabaseError::InUse)
        );
        assert!(scratch.0.join("shop.eggdb").is_file());
        drop(attached);
        served.drop_database("shop").expect("dropped");
        assert!(!scratch.0.join("shop.eggdb").exists());
        assert!(served.attach("shop").is_none());
        assert_eq!(
            served.drop_database("shop"),
            Err(ServedDatabaseError::Missing)
        );
        assert_eq!(
            served.drop_database("main"),
            Err(ServedDatabaseError::InUse)
        );
    }

    #[test]
    fn sessions_on_one_database_share_its_identity() {
        let scratch = Scratch::new("identity");
        let db = Database::open(scratch.0.join("main.eggdb")).expect("main");
        let store = DatabaseStore(&db);
        let served = serving(&scratch, &store, true);
        served.create("shop").expect("create");
        let first = served.attach("shop").expect("first");
        let second = served.attach("shop").expect("second");
        assert_eq!(first.store_id(), second.store_id());
        let main = served.attach("main").expect("main");
        assert_eq!(main.store_id(), store.store_id());
        assert_ne!(main.store_id(), first.store_id());
    }

    #[test]
    fn a_role_is_a_superuser_only_where_that_database_says_so() {
        let scratch = Scratch::new("superuser");
        let db = Database::open(scratch.0.join("main.eggdb")).expect("main");
        let store = DatabaseStore(&db);
        let served = serving(&scratch, &store, true);
        served.create("shop").expect("create");
        let mut main = eggdb_sql::Session::new(&store);
        main.execute("CREATE ROLE boss LOGIN SUPERUSER", &mut |_| {})
            .expect("role in main");
        let _ = main.close();
        assert!(served.is_superuser_in("main", "boss"));
        assert!(served.is_superuser_in("MAIN", "boss"));
        assert!(!served.is_superuser_in("shop", "boss"));
        assert!(!served.is_superuser_in("nowhere", "boss"));
        let shop = served.attach("shop").expect("shop");
        let mut session = eggdb_sql::Session::new(&shop);
        session
            .execute("CREATE ROLE boss LOGIN SUPERUSER", &mut |_| {})
            .expect("role in shop");
        let _ = session.close();
        drop(shop);
        assert!(served.is_superuser_in("Shop", "boss"));
        served
            .drop_database("shop")
            .expect("no handle is left behind");
    }

    #[test]
    fn serving_one_file_neither_creates_nor_drops() {
        let scratch = Scratch::new("single");
        let db = Database::open(scratch.0.join("main.eggdb")).expect("main");
        let store = DatabaseStore(&db);
        let served = serving(&scratch, &store, false);
        assert!(matches!(
            served.create("shop"),
            Err(ServedDatabaseError::Unsupported(_))
        ));
        assert!(!scratch.0.join("shop.eggdb").exists());
        assert_eq!(served.names(), vec!["main".to_owned()]);
    }
}

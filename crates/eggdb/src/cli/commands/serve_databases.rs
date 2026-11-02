use eggdb::Database;
use eggdb::sql::{OwnedStore, SqlSnapshot, SqlWriter, Store as DatabaseStore};
use eggdb_sql::{
    DatabaseDirectory, KeyValueStore, ServedDatabaseError, ServedDatabases, SqlResult,
    is_valid_database_name,
};
use std::collections::BTreeMap;
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

    fn changed_listing(&self) {
        self.listing_version.fetch_add(1, Ordering::AcqRel);
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
        let path = directory.join(format!("{name}.{DATABASE_EXTENSION}"));
        if std::fs::symlink_metadata(&path).is_ok() {
            return Err(ServedDatabaseError::Exists);
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
        self.changed_listing();
        Ok(())
    }

    fn drop_database(&self, name: &str) -> Result<(), ServedDatabaseError> {
        self.changing_directory()?;
        if name.eq_ignore_ascii_case(&self.default_name) {
            return Err(ServedDatabaseError::InUse);
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
        self.others()
            .get(name)
            .map(|store| AttachedDatabase::Other(Arc::clone(store)))
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
        })
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

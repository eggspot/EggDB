{
  "targets": [
    {
      "target_name": "eggdb",
      "sources": ["src/addon.c"],
      "include_dirs": ["<!(echo ${EGGDB_INCLUDE:-../../crates/eggdb-capi/include})"],
      "libraries": ["-L<!(echo ${EGGDB_LIBRARY_DIR:-../../target/release})", "-leggdb_capi"]
    }
  ]
}

# SQL:2023 conformance

EggDB's status against the feature IDs of ISO/IEC 9075-2:2023 (SQL/Foundation) and the SQL/JSON
features, as D32 asks: each feature is **done**, **partial** or **not yet**. A done feature names
the test that shows it, as `<file under crates/eggdb-sql/src>::<test function>`, and
`conformance_tests` checks that every one of those tests exists. The list grows feature by
feature; a feature not listed has not been assessed yet. Feature names follow the standard's
annex; EggDB's behavior in each dialect is described in [sql.md](sql.md).

| Feature | Name | Status | Test |
|---|---|---|---|
| E141-04 | Basic FOREIGN KEY constraint with the NO ACTION default | done | constraint_tests.rs::foreign_keys_cascade_set_null_and_restrict |
| E141-06 | CHECK constraint | done | tests.rs::inserts_check_constraints_and_return_rows |
| E152-01 | SET TRANSACTION statement: ISOLATION LEVEL SERIALIZABLE clause | done | read_only_transaction_tests.rs::show_transaction_isolation_gives_the_level_the_transaction_asked_for |
| F031 | Basic schema manipulation | done | tests.rs::create_and_drop_table_follow_postgresql |
| F191 | Referential delete actions | done | constraint_tests.rs::foreign_keys_cascade_set_null_and_restrict |
| F381 | Extended schema manipulation | done | constraint_tests.rs::alter_table_changes_columns |
| F401 | Extended joined table | done | distinct_on_and_natural_join_tests.rs::natural_joins_compare_shared_names_and_report_ambiguous_ones |
| T131 | Recursive query | done | search_cycle_tests.rs::recursive_term_wildcards_keep_the_query_columns |
| T271 | Savepoints | done | read_only_transaction_tests.rs::a_savepoint_rollback_in_a_read_only_transaction_undoes_its_temporary_writes |
| T431 | Extended grouping capabilities | done | grouping_sets_tests.rs::grouping_sets_group_by_each_set_and_leave_the_other_keys_null |
| T491 | LATERAL derived table | done | lateral_tests.rs::lateral_subqueries_and_functions_see_the_items_before_them |
| T611 | Elementary OLAP operations | done | oracle_analytic_tests.rs::aggregates_over_windows_and_ratio_to_report_share_each_partition |
| T821 | Basic SQL/JSON query operators | done | sql_json_tests.rs::json_table_builds_rows_from_columns |
| T822 | SQL/JSON: IS JSON WITH UNIQUE KEYS predicate | done | sql_json_tests.rs::is_json_tests_shape_and_unique_keys |
| S091 | Basic array support | partial | one-dimensional arrays only; multidimensional arrays are on the roadmap (M5) |
| T180 | System-versioned tables | not yet | on the roadmap (M14) |
| T181 | Application-time period tables | not yet | |
| T824 | JSON_TABLE: specific PLAN clause | not yet | on the roadmap (M6a) |
| R010 | Row pattern recognition: FROM clause | not yet | `MATCH_RECOGNIZE` is on the roadmap (M16) |

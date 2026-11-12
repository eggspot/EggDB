package app.eggspot.eggdb;

import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.ResultSet;
import java.sql.RowIdLifetime;
import java.sql.SQLException;
import java.sql.Types;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;

/**
 * What a JDBC tool asks about the database: its tables, columns, keys, indexes, routines and
 * types, read from the SQL standard's {@code information_schema} and from {@code pg_catalog}
 * through a PostgreSQL-dialect session on the same database, whatever dialect the connection
 * speaks. That session reads committed data, so objects created in the connection's own open
 * transaction appear once it commits.
 */
public final class EggDbDatabaseMetaData implements DatabaseMetaData {
    private static final String HIDDEN_SCHEMAS = "('pg_catalog', 'information_schema', 'eggdb_internal')";
    private static final String[] TABLES = {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "TABLE_TYPE", "REMARKS",
        "TYPE_CAT", "TYPE_SCHEM", "TYPE_NAME", "SELF_REFERENCING_COL_NAME", "REF_GENERATION"};
    private static final String[] COLUMNS = {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "DATA_TYPE",
        "TYPE_NAME", "COLUMN_SIZE", "BUFFER_LENGTH", "DECIMAL_DIGITS", "NUM_PREC_RADIX", "NULLABLE", "REMARKS",
        "COLUMN_DEF", "SQL_DATA_TYPE", "SQL_DATETIME_SUB", "CHAR_OCTET_LENGTH", "ORDINAL_POSITION", "IS_NULLABLE",
        "SCOPE_CATALOG", "SCOPE_SCHEMA", "SCOPE_TABLE", "SOURCE_DATA_TYPE", "IS_AUTOINCREMENT", "IS_GENERATEDCOLUMN"};
    private static final String[] PRIMARY_KEYS = {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "KEY_SEQ",
        "PK_NAME"};
    private static final String[] KEYS = {"PKTABLE_CAT", "PKTABLE_SCHEM", "PKTABLE_NAME", "PKCOLUMN_NAME",
        "FKTABLE_CAT", "FKTABLE_SCHEM", "FKTABLE_NAME", "FKCOLUMN_NAME", "KEY_SEQ", "UPDATE_RULE", "DELETE_RULE",
        "FK_NAME", "PK_NAME", "DEFERRABILITY"};
    private static final String[] INDEXES = {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "NON_UNIQUE",
        "INDEX_QUALIFIER", "INDEX_NAME", "TYPE", "ORDINAL_POSITION", "COLUMN_NAME", "ASC_OR_DESC", "CARDINALITY",
        "PAGES", "FILTER_CONDITION"};
    private static final String[] SCHEMAS = {"TABLE_SCHEM", "TABLE_CATALOG"};
    private static final String[] CATALOGS = {"TABLE_CAT"};
    private static final String[] TABLE_TYPES = {"TABLE_TYPE"};
    private static final String[] TYPE_INFO = {"TYPE_NAME", "DATA_TYPE", "PRECISION", "LITERAL_PREFIX",
        "LITERAL_SUFFIX", "CREATE_PARAMS", "NULLABLE", "CASE_SENSITIVE", "SEARCHABLE", "UNSIGNED_ATTRIBUTE",
        "FIXED_PREC_SCALE", "AUTO_INCREMENT", "LOCAL_TYPE_NAME", "MINIMUM_SCALE", "MAXIMUM_SCALE", "SQL_DATA_TYPE",
        "SQL_DATETIME_SUB", "NUM_PREC_RADIX"};
    private static final String[] PROCEDURES = {"PROCEDURE_CAT", "PROCEDURE_SCHEM", "PROCEDURE_NAME", "RESERVED1",
        "RESERVED2", "RESERVED3", "REMARKS", "PROCEDURE_TYPE", "SPECIFIC_NAME"};
    private static final String[] FUNCTIONS = {"FUNCTION_CAT", "FUNCTION_SCHEM", "FUNCTION_NAME", "REMARKS",
        "FUNCTION_TYPE", "SPECIFIC_NAME"};
    private static final String[] PROCEDURE_COLUMNS = {"PROCEDURE_CAT", "PROCEDURE_SCHEM", "PROCEDURE_NAME",
        "COLUMN_NAME", "COLUMN_TYPE", "DATA_TYPE", "TYPE_NAME", "PRECISION", "LENGTH", "SCALE", "RADIX", "NULLABLE",
        "REMARKS", "COLUMN_DEF", "SQL_DATA_TYPE", "SQL_DATETIME_SUB", "CHAR_OCTET_LENGTH", "ORDINAL_POSITION",
        "IS_NULLABLE", "SPECIFIC_NAME"};
    private static final String[] FUNCTION_COLUMNS = {"FUNCTION_CAT", "FUNCTION_SCHEM", "FUNCTION_NAME",
        "COLUMN_NAME", "COLUMN_TYPE", "DATA_TYPE", "TYPE_NAME", "PRECISION", "LENGTH", "SCALE", "RADIX", "NULLABLE",
        "REMARKS", "CHAR_OCTET_LENGTH", "ORDINAL_POSITION", "IS_NULLABLE", "SPECIFIC_NAME"};
    private static final String[] COLUMN_PRIVILEGES = {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME",
        "GRANTOR", "GRANTEE", "PRIVILEGE", "IS_GRANTABLE"};
    private static final String[] TABLE_PRIVILEGES = {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "GRANTOR", "GRANTEE",
        "PRIVILEGE", "IS_GRANTABLE"};
    private static final String[] BEST_ROW = {"SCOPE", "COLUMN_NAME", "DATA_TYPE", "TYPE_NAME", "COLUMN_SIZE",
        "BUFFER_LENGTH", "DECIMAL_DIGITS", "PSEUDO_COLUMN"};
    private static final String[] VERSION_COLUMNS = BEST_ROW;
    private static final String[] UDTS = {"TYPE_CAT", "TYPE_SCHEM", "TYPE_NAME", "CLASS_NAME", "DATA_TYPE",
        "REMARKS", "BASE_TYPE"};
    private static final String[] SUPER_TYPES = {"TYPE_CAT", "TYPE_SCHEM", "TYPE_NAME", "SUPERTYPE_CAT",
        "SUPERTYPE_SCHEM", "SUPERTYPE_NAME"};
    private static final String[] SUPER_TABLES = {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "SUPERTABLE_NAME"};
    private static final String[] ATTRIBUTES = {"TYPE_CAT", "TYPE_SCHEM", "TYPE_NAME", "ATTR_NAME", "DATA_TYPE",
        "ATTR_TYPE_NAME", "ATTR_SIZE", "DECIMAL_DIGITS", "NUM_PREC_RADIX", "NULLABLE", "REMARKS", "ATTR_DEF",
        "SQL_DATA_TYPE", "SQL_DATETIME_SUB", "CHAR_OCTET_LENGTH", "ORDINAL_POSITION", "IS_NULLABLE", "SCOPE_CATALOG",
        "SCOPE_SCHEMA", "SCOPE_TABLE", "SOURCE_DATA_TYPE"};
    private static final String[] CLIENT_INFO = {"NAME", "MAX_LEN", "DEFAULT_VALUE", "DESCRIPTION"};
    private static final String[] PSEUDO_COLUMNS = {"TABLE_CAT", "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME",
        "DATA_TYPE", "COLUMN_SIZE", "DECIMAL_DIGITS", "NUM_PREC_RADIX", "COLUMN_USAGE", "REMARKS",
        "CHAR_OCTET_LENGTH", "IS_NULLABLE"};

    private final EggDbConnection connection;

    EggDbDatabaseMetaData(EggDbConnection connection) {
        this.connection = connection;
    }

    private ResultSet result(String[] columns, List<Object[]> rows) {
        return new EggDbResultSet(new EggDbStatement(connection, null), List.of(columns), rows);
    }

    private List<Object[]> query(String sql, Object... parameters) throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        for (NativeSession.StatementResult answer : connection.catalogQuery(sql, Arrays.asList(parameters))) {
            rows.addAll(answer.rows());
        }
        return rows;
    }

    private static String pattern(String given) {
        return given == null ? "%" : given;
    }

    private static String text(Object value) {
        return value == null ? null : value.toString();
    }

    private static Long number(Object value) {
        if (value == null) {
            return null;
        }
        if (value instanceof Long whole) {
            return whole;
        }
        if (value instanceof Double real) {
            return real.longValue();
        }
        try {
            return Long.parseLong(value.toString().trim());
        } catch (NumberFormatException notNumber) {
            return null;
        }
    }

    static int jdbcType(String written) {
        if (written == null) {
            return Types.OTHER;
        }
        String type = written.toLowerCase(Locale.ROOT).trim();
        if (type.endsWith("[]") || type.equals("array")) {
            return Types.ARRAY;
        }
        return switch (type) {
            case "smallint", "int2" -> Types.SMALLINT;
            case "integer", "int", "int4" -> Types.INTEGER;
            case "bigint", "int8" -> Types.BIGINT;
            case "real", "float4" -> Types.REAL;
            case "double precision", "float8", "double", "float" -> Types.DOUBLE;
            case "numeric", "decimal" -> Types.NUMERIC;
            case "boolean", "bool" -> Types.BOOLEAN;
            case "character varying", "varchar", "text", "citext", "name" -> Types.VARCHAR;
            case "character", "char", "bpchar" -> Types.CHAR;
            case "bytea", "blob" -> Types.BINARY;
            case "date" -> Types.DATE;
            case "time without time zone", "time" -> Types.TIME;
            case "time with time zone", "timetz" -> Types.TIME_WITH_TIMEZONE;
            case "timestamp without time zone", "timestamp" -> Types.TIMESTAMP;
            case "timestamp with time zone", "timestamptz" -> Types.TIMESTAMP_WITH_TIMEZONE;
            default -> type.contains("int") ? Types.BIGINT
                    : type.contains("char") || type.contains("clob") || type.contains("text") ? Types.VARCHAR
                    : type.contains("real") || type.contains("floa") || type.contains("doub") ? Types.DOUBLE
                    : Types.OTHER;
        };
    }

    private boolean wantedType(String type, String[] types) {
        if (types == null) {
            return true;
        }
        for (String wanted : types) {
            if (wanted != null && wanted.equalsIgnoreCase(type)) {
                return true;
            }
        }
        return false;
    }

    @Override
    public ResultSet getTables(String catalog, String schemaPattern, String tableNamePattern, String[] types)
            throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        for (Object[] found : query("SELECT table_schema, table_name, table_type FROM information_schema.tables "
                + "WHERE table_schema NOT IN " + HIDDEN_SCHEMAS + " AND table_schema LIKE ? AND table_name LIKE ? "
                + "ORDER BY table_type, table_schema, table_name", pattern(schemaPattern), pattern(tableNamePattern))) {
            String type = switch (String.valueOf(found[2])) {
                case "BASE TABLE" -> "TABLE";
                case "VIEW" -> "VIEW";
                case "LOCAL TEMPORARY" -> "LOCAL TEMPORARY";
                default -> String.valueOf(found[2]);
            };
            if (wantedType(type, types)) {
                rows.add(new Object[] {null, text(found[0]), text(found[1]), type, null, null, null, null, null,
                    null});
            }
        }
        return result(TABLES, rows);
    }

    @Override
    public ResultSet getColumns(String catalog, String schemaPattern, String tableNamePattern,
            String columnNamePattern) throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        for (Object[] found : query("SELECT table_schema, table_name, column_name, data_type, "
                + "character_maximum_length, numeric_precision, numeric_scale, is_nullable, column_default, "
                + "ordinal_position, is_identity, is_generated, character_octet_length "
                + "FROM information_schema.columns WHERE table_schema NOT IN " + HIDDEN_SCHEMAS
                + " AND table_schema LIKE ? AND table_name LIKE ? AND column_name LIKE ? "
                + "ORDER BY table_schema, table_name, ordinal_position",
                pattern(schemaPattern), pattern(tableNamePattern), pattern(columnNamePattern))) {
            String typeName = text(found[3]);
            Long length = number(found[4]);
            Long precision = number(found[5]);
            boolean nullable = "YES".equals(text(found[7]));
            String defaultValue = text(found[8]);
            boolean autoIncrement = "YES".equals(text(found[10]))
                    || (defaultValue != null && defaultValue.startsWith("nextval("));
            boolean generated = "ALWAYS".equals(text(found[11]));
            rows.add(new Object[] {null, text(found[0]), text(found[1]), text(found[2]),
                (long) jdbcType(typeName), typeName, length != null ? length : precision, null, number(found[6]),
                precision != null ? 10L : null, nullable ? (long) columnNullable : (long) columnNoNulls, null,
                defaultValue, null, null, number(found[12]), number(found[9]), nullable ? "YES" : "NO", null, null,
                null, null, autoIncrement ? "YES" : "NO", generated ? "YES" : "NO"});
        }
        return result(COLUMNS, rows);
    }

    @Override
    public ResultSet getPrimaryKeys(String catalog, String schema, String table) throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        for (Object[] found : query("SELECT kcu.table_schema, kcu.table_name, kcu.column_name, "
                + "kcu.ordinal_position, tc.constraint_name FROM information_schema.table_constraints tc "
                + "JOIN information_schema.key_column_usage kcu ON kcu.constraint_schema = tc.constraint_schema "
                + "AND kcu.constraint_name = tc.constraint_name AND kcu.table_name = tc.table_name "
                + "WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema LIKE ? AND tc.table_name = ? "
                + "ORDER BY kcu.column_name", pattern(schema), table)) {
            rows.add(new Object[] {null, text(found[0]), text(found[1]), text(found[2]), number(found[3]),
                text(found[4])});
        }
        return result(PRIMARY_KEYS, rows);
    }

    private static long rule(Object written) {
        return switch (String.valueOf(written)) {
            case "CASCADE" -> importedKeyCascade;
            case "SET NULL" -> importedKeySetNull;
            case "SET DEFAULT" -> importedKeySetDefault;
            case "RESTRICT" -> importedKeyRestrict;
            default -> importedKeyNoAction;
        };
    }

    private List<Object[]> foreignKeys(String where, Object... parameters) throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        for (Object[] found : query("SELECT pk.table_schema, pk.table_name, pk.column_name, fk.table_schema, "
                + "fk.table_name, fk.column_name, fk.ordinal_position, rc.update_rule, rc.delete_rule, "
                + "rc.constraint_name, rc.unique_constraint_name "
                + "FROM information_schema.referential_constraints rc "
                + "JOIN information_schema.key_column_usage fk ON fk.constraint_schema = rc.constraint_schema "
                + "AND fk.constraint_name = rc.constraint_name "
                + "JOIN information_schema.key_column_usage pk ON pk.constraint_schema = rc.unique_constraint_schema "
                + "AND pk.constraint_name = rc.unique_constraint_name "
                + "AND pk.ordinal_position = fk.position_in_unique_constraint WHERE " + where
                + " ORDER BY pk.table_schema, pk.table_name, rc.constraint_name, fk.ordinal_position", parameters)) {
            rows.add(new Object[] {null, text(found[0]), text(found[1]), text(found[2]), null, text(found[3]),
                text(found[4]), text(found[5]), number(found[6]), rule(found[7]), rule(found[8]), text(found[9]),
                text(found[10]), (long) importedKeyNotDeferrable});
        }
        return rows;
    }

    @Override
    public ResultSet getImportedKeys(String catalog, String schema, String table) throws SQLException {
        return result(KEYS, foreignKeys("fk.table_schema LIKE ? AND fk.table_name = ?", pattern(schema), table));
    }

    @Override
    public ResultSet getExportedKeys(String catalog, String schema, String table) throws SQLException {
        return result(KEYS, foreignKeys("pk.table_schema LIKE ? AND pk.table_name = ?", pattern(schema), table));
    }

    @Override
    public ResultSet getCrossReference(String parentCatalog, String parentSchema, String parentTable,
            String foreignCatalog, String foreignSchema, String foreignTable) throws SQLException {
        return result(KEYS, foreignKeys("pk.table_schema LIKE ? AND pk.table_name = ? AND fk.table_schema LIKE ? "
                + "AND fk.table_name = ?", pattern(parentSchema), parentTable, pattern(foreignSchema), foreignTable));
    }

    @Override
    public ResultSet getIndexInfo(String catalog, String schema, String table, boolean unique, boolean approximate)
            throws SQLException {
        Map<String, Map<Long, String>> attributes = new HashMap<>();
        List<Object[]> indexes = new ArrayList<>();
        for (Object[] found : query("SELECT n.nspname, t.relname, i.relname, ix.indisunique, "
                + "CAST(ix.indkey AS text), a.attnum, a.attname FROM pg_catalog.pg_index ix "
                + "JOIN pg_catalog.pg_class i ON i.oid = ix.indexrelid "
                + "JOIN pg_catalog.pg_class t ON t.oid = ix.indrelid "
                + "JOIN pg_catalog.pg_namespace n ON n.oid = t.relnamespace "
                + "JOIN pg_catalog.pg_attribute a ON a.attrelid = t.oid AND a.attnum > 0 "
                + "WHERE n.nspname LIKE ? AND t.relname = ? ORDER BY i.relname, a.attnum", pattern(schema), table)) {
            String index = text(found[2]);
            attributes.computeIfAbsent(index, ignored -> new HashMap<>()).put(number(found[5]), text(found[6]));
            if (indexes.isEmpty() || !index.equals(indexes.get(indexes.size() - 1)[2])) {
                indexes.add(found);
            }
        }
        List<Object[]> rows = new ArrayList<>();
        for (Object[] index : indexes) {
            boolean isUnique = index[3] instanceof Long flag ? flag != 0
                    : "t".equals(text(index[3])) || "true".equals(text(index[3]));
            if (unique && !isUnique) {
                continue;
            }
            String keys = text(index[4]);
            if (keys == null || keys.isBlank()) {
                continue;
            }
            Map<Long, String> names = attributes.getOrDefault(text(index[2]), Map.of());
            long position = 0;
            for (String key : keys.trim().split("\\s+")) {
                position++;
                Long attribute = number(key);
                String column = attribute == null || attribute == 0 ? null : names.get(attribute);
                rows.add(new Object[] {null, text(index[0]), text(index[1]), !isUnique, null,
                    text(index[2]), (long) tableIndexOther, position, column, "A", null, null, null});
            }
        }
        return result(INDEXES, rows);
    }

    @Override
    public ResultSet getSchemas() throws SQLException {
        return getSchemas(null, null);
    }

    @Override
    public ResultSet getSchemas(String catalog, String schemaPattern) throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        for (Object[] found : query("SELECT schema_name FROM information_schema.schemata WHERE schema_name NOT IN "
                + HIDDEN_SCHEMAS + " AND schema_name LIKE ? ORDER BY schema_name", pattern(schemaPattern))) {
            rows.add(new Object[] {text(found[0]), null});
        }
        return result(SCHEMAS, rows);
    }

    @Override
    public ResultSet getCatalogs() {
        return result(CATALOGS, List.of());
    }

    @Override
    public ResultSet getTableTypes() {
        List<Object[]> rows = new ArrayList<>();
        rows.add(new Object[] {"LOCAL TEMPORARY"});
        rows.add(new Object[] {"TABLE"});
        rows.add(new Object[] {"VIEW"});
        return result(TABLE_TYPES, rows);
    }

    private static Object[] typeRow(String name, int type, long precision, String prefix, String suffix,
            boolean caseSensitive, boolean fixed, boolean autoIncrement) {
        return new Object[] {name, (long) type, precision, prefix, suffix, null, (long) typeNullable,
            caseSensitive, (long) typeSearchable, false, fixed, autoIncrement, name, 0L, 0L, null, null, 10L};
    }

    @Override
    public ResultSet getTypeInfo() {
        List<Object[]> rows = new ArrayList<>();
        rows.add(typeRow("bigint", Types.BIGINT, 19, null, null, false, false, true));
        rows.add(typeRow("bytea", Types.BINARY, 1_073_741_823, "'\\x", "'", false, false, false));
        rows.add(typeRow("character", Types.CHAR, 10_485_760, "'", "'", true, false, false));
        rows.add(typeRow("numeric", Types.NUMERIC, 1000, null, null, false, true, false));
        rows.add(typeRow("integer", Types.INTEGER, 10, null, null, false, false, true));
        rows.add(typeRow("smallint", Types.SMALLINT, 5, null, null, false, false, true));
        rows.add(typeRow("real", Types.REAL, 8, null, null, false, false, false));
        rows.add(typeRow("double precision", Types.DOUBLE, 17, null, null, false, false, false));
        rows.add(typeRow("character varying", Types.VARCHAR, 10_485_760, "'", "'", true, false, false));
        rows.add(typeRow("text", Types.VARCHAR, 1_073_741_823, "'", "'", true, false, false));
        rows.add(typeRow("boolean", Types.BOOLEAN, 1, null, null, false, false, false));
        rows.add(typeRow("date", Types.DATE, 13, "'", "'", false, false, false));
        rows.add(typeRow("time without time zone", Types.TIME, 15, "'", "'", false, false, false));
        rows.add(typeRow("timestamp without time zone", Types.TIMESTAMP, 29, "'", "'", false, false, false));
        rows.add(typeRow("timestamp with time zone", Types.TIMESTAMP_WITH_TIMEZONE, 35, "'", "'", false, false,
            false));
        rows.add(typeRow("uuid", Types.OTHER, 36, "'", "'", false, false, false));
        rows.add(typeRow("jsonb", Types.OTHER, 1_073_741_823, "'", "'", true, false, false));
        rows.sort((left, right) -> Long.compare((Long) left[1], (Long) right[1]));
        return result(TYPE_INFO, rows);
    }

    @Override
    public ResultSet getProcedures(String catalog, String schemaPattern, String procedureNamePattern)
            throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        for (Object[] found : query("SELECT routine_schema, routine_name, routine_type, specific_name "
                + "FROM information_schema.routines WHERE routine_schema NOT IN " + HIDDEN_SCHEMAS
                + " AND routine_schema LIKE ? AND routine_name LIKE ? ORDER BY routine_schema, routine_name",
                pattern(schemaPattern), pattern(procedureNamePattern))) {
            long type = "FUNCTION".equals(text(found[2])) ? procedureReturnsResult : procedureNoResult;
            rows.add(new Object[] {null, text(found[0]), text(found[1]), null, null, null, null, type,
                text(found[3])});
        }
        return result(PROCEDURES, rows);
    }

    @Override
    public ResultSet getFunctions(String catalog, String schemaPattern, String functionNamePattern)
            throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        for (Object[] found : query("SELECT routine_schema, routine_name, specific_name "
                + "FROM information_schema.routines WHERE routine_type = 'FUNCTION' AND routine_schema NOT IN "
                + HIDDEN_SCHEMAS + " AND routine_schema LIKE ? AND routine_name LIKE ? "
                + "ORDER BY routine_schema, routine_name", pattern(schemaPattern), pattern(functionNamePattern))) {
            rows.add(new Object[] {null, text(found[0]), text(found[1]), null, (long) functionResultUnknown,
                text(found[2])});
        }
        return result(FUNCTIONS, rows);
    }

    @Override
    public ResultSet getProcedureColumns(String catalog, String schemaPattern, String procedureNamePattern,
            String columnNamePattern) {
        return result(PROCEDURE_COLUMNS, List.of());
    }

    @Override
    public ResultSet getFunctionColumns(String catalog, String schemaPattern, String functionNamePattern,
            String columnNamePattern) {
        return result(FUNCTION_COLUMNS, List.of());
    }

    @Override
    public ResultSet getColumnPrivileges(String catalog, String schema, String table, String columnNamePattern) {
        return result(COLUMN_PRIVILEGES, List.of());
    }

    @Override
    public ResultSet getTablePrivileges(String catalog, String schemaPattern, String tableNamePattern) {
        return result(TABLE_PRIVILEGES, List.of());
    }

    @Override
    public ResultSet getBestRowIdentifier(String catalog, String schema, String table, int scope, boolean nullable)
            throws SQLException {
        List<Object[]> rows = new ArrayList<>();
        try (ResultSet keys = getPrimaryKeys(catalog, schema, table)) {
            while (keys.next()) {
                String column = keys.getString("COLUMN_NAME");
                try (ResultSet found = getColumns(catalog, schema, table, column)) {
                    if (found.next()) {
                        rows.add(new Object[] {(long) bestRowSession, column, found.getLong("DATA_TYPE"),
                            found.getString("TYPE_NAME"), found.getObject("COLUMN_SIZE"), null,
                            found.getObject("DECIMAL_DIGITS"), (long) bestRowNotPseudo});
                    }
                }
            }
        }
        return result(BEST_ROW, rows);
    }

    @Override
    public ResultSet getVersionColumns(String catalog, String schema, String table) {
        return result(VERSION_COLUMNS, List.of());
    }

    @Override
    public ResultSet getUDTs(String catalog, String schemaPattern, String typeNamePattern, int[] types) {
        return result(UDTS, List.of());
    }

    @Override
    public ResultSet getSuperTypes(String catalog, String schemaPattern, String typeNamePattern) {
        return result(SUPER_TYPES, List.of());
    }

    @Override
    public ResultSet getSuperTables(String catalog, String schemaPattern, String tableNamePattern) {
        return result(SUPER_TABLES, List.of());
    }

    @Override
    public ResultSet getAttributes(String catalog, String schemaPattern, String typeNamePattern,
            String attributeNamePattern) {
        return result(ATTRIBUTES, List.of());
    }

    @Override
    public ResultSet getClientInfoProperties() {
        return result(CLIENT_INFO, List.of());
    }

    @Override
    public ResultSet getPseudoColumns(String catalog, String schemaPattern, String tableNamePattern,
            String columnNamePattern) {
        return result(PSEUDO_COLUMNS, List.of());
    }

    @Override
    public boolean allProceduresAreCallable() {
        return true;
    }

    @Override
    public boolean allTablesAreSelectable() {
        return true;
    }

    @Override
    public String getURL() {
        return connection.url();
    }

    @Override
    public String getUserName() {
        return "eggdb";
    }

    @Override
    public boolean isReadOnly() {
        return connection.isReadOnlyConnection();
    }

    @Override
    public boolean nullsAreSortedHigh() {
        return connection.session().speaksPostgresql();
    }

    @Override
    public boolean nullsAreSortedLow() {
        return !connection.session().speaksPostgresql();
    }

    @Override
    public boolean nullsAreSortedAtStart() {
        return false;
    }

    @Override
    public boolean nullsAreSortedAtEnd() {
        return false;
    }

    @Override
    public String getDatabaseProductName() {
        return "EggDB";
    }

    @Override
    public String getDatabaseProductVersion() {
        return "0.5.0";
    }

    @Override
    public String getDriverName() {
        return "EggDB JDBC";
    }

    @Override
    public String getDriverVersion() {
        return "0.1";
    }

    @Override
    public int getDriverMajorVersion() {
        return 0;
    }

    @Override
    public int getDriverMinorVersion() {
        return 1;
    }

    @Override
    public boolean usesLocalFiles() {
        return true;
    }

    @Override
    public boolean usesLocalFilePerTable() {
        return false;
    }

    @Override
    public boolean supportsMixedCaseIdentifiers() {
        return false;
    }

    @Override
    public boolean storesUpperCaseIdentifiers() {
        return false;
    }

    @Override
    public boolean storesLowerCaseIdentifiers() {
        return true;
    }

    @Override
    public boolean storesMixedCaseIdentifiers() {
        return false;
    }

    @Override
    public boolean supportsMixedCaseQuotedIdentifiers() {
        return true;
    }

    @Override
    public boolean storesUpperCaseQuotedIdentifiers() {
        return false;
    }

    @Override
    public boolean storesLowerCaseQuotedIdentifiers() {
        return false;
    }

    @Override
    public boolean storesMixedCaseQuotedIdentifiers() {
        return false;
    }

    @Override
    public String getIdentifierQuoteString() {
        return "\"";
    }

    @Override
    public String getSQLKeywords() {
        return "LIMIT,OFFSET,RETURNING,ILIKE,ANALYZE,VACUUM";
    }

    @Override
    public String getNumericFunctions() {
        return "abs,ceil,ceiling,exp,floor,ln,log,mod,power,round,sign,sqrt,trunc";
    }

    @Override
    public String getStringFunctions() {
        return "concat,length,lower,ltrim,replace,rtrim,substring,trim,upper";
    }

    @Override
    public String getSystemFunctions() {
        return "coalesce,nullif";
    }

    @Override
    public String getTimeDateFunctions() {
        return "current_date,current_time,current_timestamp,extract,now";
    }

    @Override
    public String getSearchStringEscape() {
        return "\\";
    }

    @Override
    public String getExtraNameCharacters() {
        return "";
    }

    @Override
    public boolean supportsAlterTableWithAddColumn() {
        return true;
    }

    @Override
    public boolean supportsAlterTableWithDropColumn() {
        return true;
    }

    @Override
    public boolean supportsColumnAliasing() {
        return true;
    }

    @Override
    public boolean nullPlusNonNullIsNull() {
        return true;
    }

    @Override
    public boolean supportsConvert() {
        return false;
    }

    @Override
    public boolean supportsConvert(int fromType, int toType) {
        return false;
    }

    @Override
    public boolean supportsTableCorrelationNames() {
        return true;
    }

    @Override
    public boolean supportsDifferentTableCorrelationNames() {
        return false;
    }

    @Override
    public boolean supportsExpressionsInOrderBy() {
        return true;
    }

    @Override
    public boolean supportsOrderByUnrelated() {
        return true;
    }

    @Override
    public boolean supportsGroupBy() {
        return true;
    }

    @Override
    public boolean supportsGroupByUnrelated() {
        return true;
    }

    @Override
    public boolean supportsGroupByBeyondSelect() {
        return true;
    }

    @Override
    public boolean supportsLikeEscapeClause() {
        return true;
    }

    @Override
    public boolean supportsMultipleResultSets() {
        return true;
    }

    @Override
    public boolean supportsMultipleTransactions() {
        return true;
    }

    @Override
    public boolean supportsNonNullableColumns() {
        return true;
    }

    @Override
    public boolean supportsMinimumSQLGrammar() {
        return true;
    }

    @Override
    public boolean supportsCoreSQLGrammar() {
        return true;
    }

    @Override
    public boolean supportsExtendedSQLGrammar() {
        return false;
    }

    @Override
    public boolean supportsANSI92EntryLevelSQL() {
        return true;
    }

    @Override
    public boolean supportsANSI92IntermediateSQL() {
        return false;
    }

    @Override
    public boolean supportsANSI92FullSQL() {
        return false;
    }

    @Override
    public boolean supportsIntegrityEnhancementFacility() {
        return true;
    }

    @Override
    public boolean supportsOuterJoins() {
        return true;
    }

    @Override
    public boolean supportsFullOuterJoins() {
        return true;
    }

    @Override
    public boolean supportsLimitedOuterJoins() {
        return true;
    }

    @Override
    public String getSchemaTerm() {
        return "schema";
    }

    @Override
    public String getProcedureTerm() {
        return "function";
    }

    @Override
    public String getCatalogTerm() {
        return "database";
    }

    @Override
    public boolean isCatalogAtStart() {
        return true;
    }

    @Override
    public String getCatalogSeparator() {
        return ".";
    }

    @Override
    public boolean supportsSchemasInDataManipulation() {
        return true;
    }

    @Override
    public boolean supportsSchemasInProcedureCalls() {
        return true;
    }

    @Override
    public boolean supportsSchemasInTableDefinitions() {
        return true;
    }

    @Override
    public boolean supportsSchemasInIndexDefinitions() {
        return true;
    }

    @Override
    public boolean supportsSchemasInPrivilegeDefinitions() {
        return true;
    }

    @Override
    public boolean supportsCatalogsInDataManipulation() {
        return false;
    }

    @Override
    public boolean supportsCatalogsInProcedureCalls() {
        return false;
    }

    @Override
    public boolean supportsCatalogsInTableDefinitions() {
        return false;
    }

    @Override
    public boolean supportsCatalogsInIndexDefinitions() {
        return false;
    }

    @Override
    public boolean supportsCatalogsInPrivilegeDefinitions() {
        return false;
    }

    @Override
    public boolean supportsPositionedDelete() {
        return false;
    }

    @Override
    public boolean supportsPositionedUpdate() {
        return false;
    }

    @Override
    public boolean supportsSelectForUpdate() {
        return true;
    }

    @Override
    public boolean supportsStoredProcedures() {
        return true;
    }

    @Override
    public boolean supportsSubqueriesInComparisons() {
        return true;
    }

    @Override
    public boolean supportsSubqueriesInExists() {
        return true;
    }

    @Override
    public boolean supportsSubqueriesInIns() {
        return true;
    }

    @Override
    public boolean supportsSubqueriesInQuantifieds() {
        return true;
    }

    @Override
    public boolean supportsCorrelatedSubqueries() {
        return true;
    }

    @Override
    public boolean supportsUnion() {
        return true;
    }

    @Override
    public boolean supportsUnionAll() {
        return true;
    }

    @Override
    public boolean supportsOpenCursorsAcrossCommit() {
        return false;
    }

    @Override
    public boolean supportsOpenCursorsAcrossRollback() {
        return false;
    }

    @Override
    public boolean supportsOpenStatementsAcrossCommit() {
        return true;
    }

    @Override
    public boolean supportsOpenStatementsAcrossRollback() {
        return true;
    }

    @Override
    public int getMaxBinaryLiteralLength() {
        return 0;
    }

    @Override
    public int getMaxCharLiteralLength() {
        return 0;
    }

    @Override
    public int getMaxColumnNameLength() {
        return 63;
    }

    @Override
    public int getMaxColumnsInGroupBy() {
        return 0;
    }

    @Override
    public int getMaxColumnsInIndex() {
        return 32;
    }

    @Override
    public int getMaxColumnsInOrderBy() {
        return 0;
    }

    @Override
    public int getMaxColumnsInSelect() {
        return 0;
    }

    @Override
    public int getMaxColumnsInTable() {
        return 1600;
    }

    @Override
    public int getMaxConnections() {
        return 0;
    }

    @Override
    public int getMaxCursorNameLength() {
        return 63;
    }

    @Override
    public int getMaxIndexLength() {
        return 0;
    }

    @Override
    public int getMaxSchemaNameLength() {
        return 63;
    }

    @Override
    public int getMaxProcedureNameLength() {
        return 63;
    }

    @Override
    public int getMaxCatalogNameLength() {
        return 63;
    }

    @Override
    public int getMaxRowSize() {
        return 0;
    }

    @Override
    public boolean doesMaxRowSizeIncludeBlobs() {
        return false;
    }

    @Override
    public int getMaxStatementLength() {
        return 0;
    }

    @Override
    public int getMaxStatements() {
        return 0;
    }

    @Override
    public int getMaxTableNameLength() {
        return 63;
    }

    @Override
    public int getMaxTablesInSelect() {
        return 0;
    }

    @Override
    public int getMaxUserNameLength() {
        return 63;
    }

    @Override
    public int getDefaultTransactionIsolation() {
        return Connection.TRANSACTION_SERIALIZABLE;
    }

    @Override
    public boolean supportsTransactions() {
        return true;
    }

    @Override
    public boolean supportsTransactionIsolationLevel(int level) {
        return level == Connection.TRANSACTION_SERIALIZABLE;
    }

    @Override
    public boolean supportsDataDefinitionAndDataManipulationTransactions() {
        return true;
    }

    @Override
    public boolean supportsDataManipulationTransactionsOnly() {
        return false;
    }

    @Override
    public boolean dataDefinitionCausesTransactionCommit() {
        return false;
    }

    @Override
    public boolean dataDefinitionIgnoredInTransactions() {
        return false;
    }

    @Override
    public boolean supportsResultSetType(int type) {
        return type == ResultSet.TYPE_FORWARD_ONLY;
    }

    @Override
    public boolean supportsResultSetConcurrency(int type, int concurrency) {
        return type == ResultSet.TYPE_FORWARD_ONLY && concurrency == ResultSet.CONCUR_READ_ONLY;
    }

    @Override
    public boolean ownUpdatesAreVisible(int type) {
        return false;
    }

    @Override
    public boolean ownDeletesAreVisible(int type) {
        return false;
    }

    @Override
    public boolean ownInsertsAreVisible(int type) {
        return false;
    }

    @Override
    public boolean othersUpdatesAreVisible(int type) {
        return false;
    }

    @Override
    public boolean othersDeletesAreVisible(int type) {
        return false;
    }

    @Override
    public boolean othersInsertsAreVisible(int type) {
        return false;
    }

    @Override
    public boolean updatesAreDetected(int type) {
        return false;
    }

    @Override
    public boolean deletesAreDetected(int type) {
        return false;
    }

    @Override
    public boolean insertsAreDetected(int type) {
        return false;
    }

    @Override
    public boolean supportsBatchUpdates() {
        return true;
    }

    @Override
    public Connection getConnection() {
        return connection;
    }

    @Override
    public boolean supportsSavepoints() {
        return true;
    }

    @Override
    public boolean supportsNamedParameters() {
        return false;
    }

    @Override
    public boolean supportsMultipleOpenResults() {
        return false;
    }

    @Override
    public boolean supportsGetGeneratedKeys() {
        return false;
    }

    @Override
    public boolean supportsResultSetHoldability(int holdability) {
        return holdability == ResultSet.CLOSE_CURSORS_AT_COMMIT;
    }

    @Override
    public int getResultSetHoldability() {
        return ResultSet.CLOSE_CURSORS_AT_COMMIT;
    }

    @Override
    public int getDatabaseMajorVersion() {
        return 0;
    }

    @Override
    public int getDatabaseMinorVersion() {
        return 5;
    }

    @Override
    public int getJDBCMajorVersion() {
        return 4;
    }

    @Override
    public int getJDBCMinorVersion() {
        return 3;
    }

    @Override
    public int getSQLStateType() {
        return sqlStateSQL;
    }

    @Override
    public boolean locatorsUpdateCopy() {
        return true;
    }

    @Override
    public boolean supportsStatementPooling() {
        return false;
    }

    @Override
    public RowIdLifetime getRowIdLifetime() {
        return RowIdLifetime.ROWID_UNSUPPORTED;
    }

    @Override
    public boolean supportsStoredFunctionsUsingCallSyntax() {
        return false;
    }

    @Override
    public boolean autoCommitFailureClosesAllResultSets() {
        return false;
    }

    @Override
    public boolean generatedKeyAlwaysReturned() {
        return false;
    }

    @Override
    public <T> T unwrap(Class<T> type) throws SQLException {
        if (type.isInstance(this)) {
            return type.cast(this);
        }
        throw new SQLException("not a wrapper for " + type.getName(), "HY000");
    }

    @Override
    public boolean isWrapperFor(Class<?> type) {
        return type.isInstance(this);
    }
}

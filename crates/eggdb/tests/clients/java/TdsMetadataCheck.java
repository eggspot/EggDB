import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;

public final class TdsMetadataCheck {
    private interface Step {
        void run(DatabaseMetaData meta) throws SQLException;
    }

    private static final Set<String> SHOWN_TYPES = Set.of("int", "bigint", "nvarchar", "decimal", "bit",
            "datetime2", "uniqueidentifier");

    private TdsMetadataCheck() {
    }

    public static void main(String[] arguments) throws SQLException {
        if (arguments.length != 3) {
            System.err.println("usage: java -cp mssql-jdbc.jar TdsMetadataCheck.java <jdbc url> <user> <password>");
            System.exit(2);
        }
        try (Connection connection = DriverManager.getConnection(arguments[0], arguments[1], arguments[2])) {
            DatabaseMetaData meta = connection.getMetaData();
            step(meta, "catalogs", TdsMetadataCheck::catalogs);
            step(meta, "user", TdsMetadataCheck::user);
            step(meta, "tables", TdsMetadataCheck::tables);
            step(meta, "columns of dbo.customers", m -> columns(m, "dbo", "customers"));
            step(meta, "columns of sales.orders", m -> columns(m, "sales", "orders"));
            step(meta, "primary keys of sales.orders", TdsMetadataCheck::primaryKeys);
            step(meta, "imported keys of sales.orders", TdsMetadataCheck::importedKeys);
            step(meta, "index info of sales.orders", TdsMetadataCheck::indexInfo);
            step(meta, "type info", TdsMetadataCheck::typeInfo);
        }
    }

    private static void step(DatabaseMetaData meta, String name, Step body) {
        System.out.println("== " + name);
        try {
            body.run(meta);
        } catch (SQLException failure) {
            System.out.println("error " + failure.getErrorCode() + " " + failure.getMessage());
        }
    }

    private static String shown(Object value) {
        return value == null ? "NULL" : value.toString();
    }

    private static void print(ResultSet rows, String... columns) throws SQLException {
        try (rows) {
            while (rows.next()) {
                List<String> cells = new ArrayList<>();
                for (String column : columns) {
                    cells.add(shown(rows.getObject(column)));
                }
                System.out.println(String.join("|", cells));
            }
        }
    }

    private static void catalogs(DatabaseMetaData meta) throws SQLException {
        print(meta.getCatalogs(), "TABLE_CAT");
    }

    private static void user(DatabaseMetaData meta) throws SQLException {
        System.out.println(meta.getUserName());
    }

    private static void tables(DatabaseMetaData meta) throws SQLException {
        print(meta.getTables(null, null, "%", new String[] {"TABLE", "VIEW"}), "TABLE_CAT", "TABLE_SCHEM",
                "TABLE_NAME", "TABLE_TYPE");
    }

    private static void columns(DatabaseMetaData meta, String schema, String table) throws SQLException {
        print(meta.getColumns(null, schema, table, null), "COLUMN_NAME", "DATA_TYPE", "TYPE_NAME", "COLUMN_SIZE",
                "DECIMAL_DIGITS", "NULLABLE", "COLUMN_DEF", "ORDINAL_POSITION", "IS_NULLABLE", "IS_AUTOINCREMENT");
    }

    private static void primaryKeys(DatabaseMetaData meta) throws SQLException {
        print(meta.getPrimaryKeys(null, "sales", "orders"), "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "KEY_SEQ",
                "PK_NAME");
    }

    private static void importedKeys(DatabaseMetaData meta) throws SQLException {
        print(meta.getImportedKeys(null, "sales", "orders"), "PKTABLE_SCHEM", "PKTABLE_NAME", "PKCOLUMN_NAME",
                "FKTABLE_SCHEM", "FKTABLE_NAME", "FKCOLUMN_NAME", "KEY_SEQ", "UPDATE_RULE", "DELETE_RULE", "FK_NAME",
                "PK_NAME");
    }

    private static void indexInfo(DatabaseMetaData meta) throws SQLException {
        print(meta.getIndexInfo(null, "sales", "orders", false, true), "TYPE", "NON_UNIQUE", "INDEX_NAME",
                "ORDINAL_POSITION", "COLUMN_NAME", "ASC_OR_DESC");
    }

    private static void typeInfo(DatabaseMetaData meta) throws SQLException {
        try (ResultSet rows = meta.getTypeInfo()) {
            int count = 0;
            while (rows.next()) {
                count++;
                String name = rows.getString("TYPE_NAME");
                if (SHOWN_TYPES.contains(name)) {
                    System.out.println(name + "|" + rows.getInt("DATA_TYPE") + "|" + rows.getInt("PRECISION"));
                }
            }
            System.out.println(count > 20 ? "more than 20 types" : "only " + count + " types");
        }
    }
}

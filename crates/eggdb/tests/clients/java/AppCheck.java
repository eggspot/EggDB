import java.math.BigDecimal;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.Date;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Statement;
import java.util.ArrayList;
import java.util.List;

public final class AppCheck {
    private interface Step {
        void run(Connection connection) throws SQLException;
    }

    private AppCheck() {
    }

    public static void main(String[] arguments) throws SQLException {
        if (arguments.length != 3) {
            System.err.println("usage: java -cp postgresql.jar AppCheck.java <jdbc url> <user> <password>");
            System.exit(2);
        }
        try (Connection connection = DriverManager.getConnection(arguments[0], arguments[1], arguments[2])) {
            step(connection, "product", AppCheck::product);
            step(connection, "isolation", AppCheck::isolation);
            step(connection, "crud", AppCheck::crud);
            step(connection, "transaction", AppCheck::transaction);
            step(connection, "fixture rows", AppCheck::fixtureRows);
            step(connection, "schemas", AppCheck::schemas);
            step(connection, "tables", AppCheck::tables);
            step(connection, "columns", AppCheck::columns);
            step(connection, "primary keys", AppCheck::primaryKeys);
            step(connection, "imported keys", AppCheck::importedKeys);
        }
    }

    private static void step(Connection connection, String name, Step body) {
        System.out.println("== " + name);
        try {
            body.run(connection);
        } catch (SQLException error) {
            System.out.println("error " + error.getSQLState());
            try {
                if (!connection.getAutoCommit()) {
                    connection.rollback();
                    connection.setAutoCommit(true);
                }
            } catch (SQLException failure) {
                System.out.println("rollback failed " + failure.getSQLState());
            }
        }
    }

    private static String shown(Object value) {
        return value == null ? "NULL" : value.toString();
    }

    private static void print(ResultSet rows, String... columns) throws SQLException {
        while (rows.next()) {
            List<String> cells = new ArrayList<>();
            for (String column : columns) {
                cells.add(shown(rows.getObject(column)));
            }
            System.out.println(String.join("\t", cells));
        }
    }

    private static void printAll(ResultSet rows) throws SQLException {
        ResultSetMetaData shape = rows.getMetaData();
        List<String> names = new ArrayList<>();
        for (int at = 1; at <= shape.getColumnCount(); at++) {
            names.add(shape.getColumnLabel(at));
        }
        System.out.println(String.join("\t", names));
        while (rows.next()) {
            List<String> cells = new ArrayList<>();
            for (int at = 1; at <= shape.getColumnCount(); at++) {
                cells.add(shown(rows.getObject(at)));
            }
            System.out.println(String.join("\t", cells));
        }
    }

    private static void product(Connection connection) throws SQLException {
        DatabaseMetaData metadata = connection.getMetaData();
        System.out.println(metadata.getDatabaseProductName());
        System.out.println(metadata.getMaxIndexKeys() >= 0);
    }

    private static void isolation(Connection connection) throws SQLException {
        connection.setTransactionIsolation(Connection.TRANSACTION_SERIALIZABLE);
        System.out.println(connection.getTransactionIsolation() == Connection.TRANSACTION_SERIALIZABLE);
        connection.setTransactionIsolation(Connection.TRANSACTION_READ_COMMITTED);
        connection.setTransactionIsolation(Connection.TRANSACTION_SERIALIZABLE);
    }

    private static void crud(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement()) {
            statement.execute("DROP TABLE IF EXISTS app_notes");
            statement.execute("CREATE TABLE app_notes (id serial PRIMARY KEY, title varchar(80) NOT NULL, "
                    + "amount numeric(10,2), due date, done boolean NOT NULL DEFAULT false, weight double precision)");
        }
        try (PreparedStatement insert = connection.prepareStatement(
                "INSERT INTO app_notes (title, amount, due, done, weight) VALUES (?, ?, ?, ?, ?)")) {
            for (int number = 1; number <= 7; number++) {
                insert.setString(1, "note " + number);
                insert.setBigDecimal(2, new BigDecimal(number + ".25"));
                insert.setDate(3, Date.valueOf("2026-10-" + (10 + number)));
                insert.setBoolean(4, number % 2 == 0);
                insert.setDouble(5, number / 4.0);
                System.out.println("inserted " + insert.executeUpdate());
            }
        }
        try (PreparedStatement returning = connection.prepareStatement(
                "INSERT INTO app_notes (title) VALUES (?) RETURNING id, done")) {
            returning.setString(1, "returned");
            try (ResultSet rows = returning.executeQuery()) {
                print(rows, "id", "done");
            }
        }
        try (PreparedStatement select = connection.prepareStatement(
                "SELECT id, title, amount, due, done, weight FROM app_notes WHERE amount > ? AND title LIKE ? ORDER BY id")) {
            for (int round = 0; round < 6; round++) {
                select.setBigDecimal(1, new BigDecimal("3.00"));
                select.setString(2, "note%");
                try (ResultSet rows = select.executeQuery()) {
                    if (round == 5) {
                        printAll(rows);
                    }
                }
            }
        }
        try (PreparedStatement update = connection.prepareStatement(
                "UPDATE app_notes SET amount = amount * ?, done = ? WHERE id <= ?")) {
            update.setInt(1, 2);
            update.setBoolean(2, true);
            update.setLong(3, 3L);
            System.out.println("updated " + update.executeUpdate());
        }
        try (PreparedStatement delete = connection.prepareStatement("DELETE FROM app_notes WHERE done = ?")) {
            delete.setBoolean(1, false);
            System.out.println("deleted " + delete.executeUpdate());
        }
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery(
                        "SELECT id, title, amount, due, done FROM app_notes ORDER BY id")) {
            printAll(rows);
        }
    }

    private static int countNotes(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery("SELECT count(*) FROM app_notes")) {
            rows.next();
            return rows.getInt(1);
        }
    }

    private static void transaction(Connection connection) throws SQLException {
        connection.setAutoCommit(false);
        try (PreparedStatement insert = connection.prepareStatement("INSERT INTO app_notes (title) VALUES (?)")) {
            insert.setString(1, "rolled back");
            insert.executeUpdate();
            System.out.println("inside " + countNotes(connection));
            connection.rollback();
            System.out.println("after rollback " + countNotes(connection));
            insert.setString(1, "committed");
            insert.executeUpdate();
            connection.commit();
        }
        connection.setAutoCommit(true);
        System.out.println("after commit " + countNotes(connection));
        try (Statement statement = connection.createStatement()) {
            statement.execute("DROP TABLE app_notes");
        }
    }

    private static void fixtureRows(Connection connection) throws SQLException {
        try (PreparedStatement select = connection.prepareStatement(
                "SELECT c.id, c.name, c.email, c.country, c.active, count(o.id) AS orders, sum(o.total) AS total "
                        + "FROM customers c LEFT JOIN orders o ON o.customer_id = c.id "
                        + "WHERE c.id <= ? GROUP BY c.id, c.name, c.email, c.country, c.active ORDER BY c.id")) {
            select.setInt(1, 4);
            try (ResultSet rows = select.executeQuery()) {
                printAll(rows);
            }
        }
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery(
                        "SELECT code, name, opens FROM shop.warehouses ORDER BY code")) {
            printAll(rows);
        }
    }

    private static void schemas(Connection connection) throws SQLException {
        try (ResultSet rows = connection.getMetaData().getSchemas()) {
            print(rows, "TABLE_SCHEM");
        }
    }

    private static void tables(Connection connection) throws SQLException {
        try (ResultSet rows = connection.getMetaData().getTables(null, "%", "%", new String[] {"TABLE", "VIEW"})) {
            print(rows, "TABLE_SCHEM", "TABLE_NAME", "TABLE_TYPE", "REMARKS");
        }
    }

    private static void columns(Connection connection) throws SQLException {
        for (String schema : new String[] {"public", "shop"}) {
            try (ResultSet rows = connection.getMetaData().getColumns(null, schema, "%", "%")) {
                print(rows, "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "DATA_TYPE", "TYPE_NAME", "COLUMN_SIZE",
                        "DECIMAL_DIGITS", "NULLABLE", "REMARKS", "COLUMN_DEF", "ORDINAL_POSITION", "IS_NULLABLE",
                        "IS_AUTOINCREMENT");
            }
        }
    }

    private static void primaryKeys(Connection connection) throws SQLException {
        String[][] tables = {{"public", "orders"}, {"public", "order_items"}, {"shop", "stock"}};
        for (String[] table : tables) {
            try (ResultSet rows = connection.getMetaData().getPrimaryKeys(null, table[0], table[1])) {
                print(rows, "TABLE_SCHEM", "TABLE_NAME", "COLUMN_NAME", "KEY_SEQ", "PK_NAME");
            }
        }
    }

    private static void importedKeys(Connection connection) throws SQLException {
        String[][] tables = {{"public", "orders"}, {"public", "order_items"}, {"shop", "stock"}};
        for (String[] table : tables) {
            try (ResultSet rows = connection.getMetaData().getImportedKeys(null, table[0], table[1])) {
                print(rows, "PKTABLE_SCHEM", "PKTABLE_NAME", "PKCOLUMN_NAME", "FKTABLE_SCHEM", "FKTABLE_NAME",
                        "FKCOLUMN_NAME", "KEY_SEQ", "UPDATE_RULE", "DELETE_RULE", "FK_NAME", "PK_NAME",
                        "DEFERRABILITY");
            }
        }
    }
}

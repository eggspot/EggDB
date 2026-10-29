import java.math.BigDecimal;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;
import java.sql.Types;
import java.time.LocalDateTime;
import java.time.format.DateTimeFormatter;
import java.util.ArrayList;
import java.util.List;
import java.util.Locale;

public final class TdsAppCheck {
    private interface Step {
        void run(Connection connection) throws SQLException;
    }

    private static final DateTimeFormatter MOMENT = DateTimeFormatter.ofPattern("yyyy-MM-dd HH:mm:ss.SSS");

    private TdsAppCheck() {
    }

    public static void main(String[] arguments) throws SQLException {
        if (arguments.length != 3) {
            System.err.println("usage: java -cp mssql-jdbc.jar TdsAppCheck.java <jdbc url> <user> <password>");
            System.exit(2);
        }
        try (Connection connection = DriverManager.getConnection(arguments[0], arguments[1], arguments[2])) {
            step(connection, "setup", TdsAppCheck::setup);
            step(connection, "insert", TdsAppCheck::insert);
            step(connection, "select", TdsAppCheck::select);
            step(connection, "update", TdsAppCheck::update);
            step(connection, "commit", TdsAppCheck::commit);
            step(connection, "rollback", TdsAppCheck::rollback);
            step(connection, "delete", TdsAppCheck::delete);
            step(connection, "error", TdsAppCheck::error);
        }
    }

    private static void step(Connection connection, String name, Step body) {
        System.out.println("== " + name);
        try {
            body.run(connection);
        } catch (SQLException failure) {
            System.out.println("error " + failure.getErrorCode() + " " + failure.getMessage());
            try {
                if (!connection.getAutoCommit()) {
                    connection.rollback();
                    connection.setAutoCommit(true);
                }
            } catch (SQLException again) {
                System.out.println("rollback failed " + again.getErrorCode());
            }
        }
    }

    private static String shown(Object value) {
        if (value == null) {
            return "NULL";
        }
        if (value instanceof BigDecimal number) {
            return number.toPlainString();
        }
        if (value instanceof Boolean flag) {
            return flag ? "1" : "0";
        }
        if (value instanceof LocalDateTime moment) {
            return moment.format(MOMENT);
        }
        return value.toString();
    }

    private static void setup(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement()) {
            statement.execute("DROP TABLE IF EXISTS bakes");
            statement.execute("CREATE TABLE bakes (id INT PRIMARY KEY, name NVARCHAR(40) NOT NULL, "
                    + "price DECIMAL(10,2), baked DATETIME2(3), flag BIT NOT NULL, code UNIQUEIDENTIFIER)");
        }
        System.out.println("ready");
    }

    private static void insertOne(PreparedStatement insert, int id, String name, String price, String baked,
            boolean flag, String code) throws SQLException {
        insert.setInt(1, id);
        insert.setString(2, name);
        if (price == null) {
            insert.setNull(3, Types.DECIMAL);
        } else {
            insert.setBigDecimal(3, new BigDecimal(price));
        }
        if (baked == null) {
            insert.setNull(4, Types.TIMESTAMP);
        } else {
            insert.setObject(4, LocalDateTime.parse(baked));
        }
        insert.setBoolean(5, flag);
        if (code == null) {
            insert.setNull(6, Types.CHAR);
        } else {
            insert.setString(6, code);
        }
        System.out.println("inserted " + insert.executeUpdate());
    }

    private static PreparedStatement inserting(Connection connection) throws SQLException {
        return connection.prepareStatement(
                "INSERT INTO bakes (id, name, price, baked, flag, code) VALUES (?, ?, ?, ?, ?, ?)");
    }

    private static void insert(Connection connection) throws SQLException {
        try (PreparedStatement insert = inserting(connection)) {
            insertOne(insert, 1, "bread", "12.50", "2024-05-06T07:08:09.123", true,
                    "6F9619FF-8B86-D011-B42D-00C04FC964FF");
            insertOne(insert, 2, "egg", "0.35", "2024-05-06T08:00:00", false, null);
            insertOne(insert, 3, "pie", null, null, true, "1B4E28BA-2FA1-11D2-883F-0016D3CCA427");
        }
    }

    private static void printRows(Connection connection, int lowest) throws SQLException {
        try (PreparedStatement select = connection.prepareStatement(
                "SELECT id, name, price, baked, flag, code FROM bakes WHERE id >= ? ORDER BY id")) {
            select.setInt(1, lowest);
            try (ResultSet rows = select.executeQuery()) {
                while (rows.next()) {
                    List<String> cells = new ArrayList<>();
                    cells.add(shown(rows.getObject(1)));
                    cells.add(shown(rows.getString(2)));
                    cells.add(shown(rows.getBigDecimal(3)));
                    cells.add(shown(rows.getObject(4, LocalDateTime.class)));
                    cells.add(shown(rows.getBoolean(5)));
                    String code = rows.getString(6);
                    cells.add(code == null ? "NULL" : code.toUpperCase(Locale.ROOT));
                    System.out.println(String.join("|", cells));
                }
            }
        }
    }

    private static void count(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery("SELECT COUNT(*) AS n FROM bakes")) {
            rows.next();
            System.out.println("count " + rows.getInt(1));
        }
    }

    private static void select(Connection connection) throws SQLException {
        printRows(connection, 1);
    }

    private static void update(Connection connection) throws SQLException {
        try (PreparedStatement update = connection.prepareStatement("UPDATE bakes SET price = ? WHERE name = ?")) {
            update.setBigDecimal(1, new BigDecimal("13.00"));
            update.setString(2, "bread");
            System.out.println("updated " + update.executeUpdate());
        }
        printRows(connection, 1);
    }

    private static void commit(Connection connection) throws SQLException {
        connection.setAutoCommit(false);
        try (PreparedStatement insert = inserting(connection)) {
            insertOne(insert, 4, "tart", "4.20", "2024-05-07T09:30:00", false, null);
        }
        connection.commit();
        connection.setAutoCommit(true);
        count(connection);
    }

    private static void rollback(Connection connection) throws SQLException {
        connection.setAutoCommit(false);
        try (PreparedStatement insert = inserting(connection)) {
            insertOne(insert, 5, "scone", "2.00", null, true, null);
        }
        connection.rollback();
        connection.setAutoCommit(true);
        count(connection);
    }

    private static void delete(Connection connection) throws SQLException {
        try (PreparedStatement delete = connection.prepareStatement("DELETE FROM bakes WHERE id = ?")) {
            delete.setInt(1, 3);
            System.out.println("deleted " + delete.executeUpdate());
        }
        printRows(connection, 1);
    }

    private static void error(Connection connection) throws SQLException {
        try (PreparedStatement missing = connection.prepareStatement("SELECT * FROM missing_table WHERE id = ?")) {
            missing.setInt(1, 1);
            missing.executeQuery().close();
        }
    }
}

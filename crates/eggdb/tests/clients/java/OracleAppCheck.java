import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;
import java.sql.Timestamp;
import java.util.ArrayList;
import java.util.List;
import java.util.Properties;

public final class OracleAppCheck {
    private interface Step {
        void run(Connection connection) throws SQLException;
    }

    private OracleAppCheck() {
    }

    public static void main(String[] arguments) throws SQLException {
        if (arguments.length != 3) {
            System.err.println("usage: java -cp postgresql.jar OracleAppCheck.java <jdbc url> <user> <password>");
            System.exit(2);
        }
        Properties properties = new Properties();
        properties.setProperty("user", arguments[1]);
        properties.setProperty("password", arguments[2]);
        properties.setProperty("options", "-c eggdb.dialect=oracle");
        try (Connection connection = DriverManager.getConnection(arguments[0], properties)) {
            step(connection, "dictionary", OracleAppCheck::dictionary);
            step(connection, "session", OracleAppCheck::session);
            step(connection, "department", OracleAppCheck::department);
            step(connection, "join", OracleAppCheck::join);
            step(connection, "hire date", OracleAppCheck::hireDate);
            step(connection, "transaction", OracleAppCheck::transaction);
            step(connection, "missing table", OracleAppCheck::missingTable);
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

    private static void print(ResultSet rows, int columns) throws SQLException {
        while (rows.next()) {
            List<String> cells = new ArrayList<>();
            for (int at = 1; at <= columns; at++) {
                String value = rows.getString(at);
                cells.add(value == null ? "NULL" : value);
            }
            System.out.println(String.join("\t", cells));
        }
    }

    private static long count(Connection connection, String table) throws SQLException {
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery("SELECT COUNT(*) FROM " + table)) {
            rows.next();
            return rows.getLong(1);
        }
    }

    private static void dictionary(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery("SELECT table_name FROM user_tables ORDER BY table_name")) {
            print(rows, 1);
        }
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery(
                        "SELECT c.table_name, c.constraint_name, p.table_name FROM user_constraints c "
                                + "JOIN user_constraints p ON p.constraint_name = c.r_constraint_name "
                                + "WHERE c.constraint_type = 'R' AND c.table_name = 'DEPARTMENTS' ORDER BY c.constraint_name")) {
            print(rows, 3);
        }
    }

    private static void session(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery(
                        "SELECT SYS_CONTEXT('USERENV', 'SESSION_USER'), SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual")) {
            print(rows, 2);
        }
        try (Statement statement = connection.createStatement();
                ResultSet rows = statement.executeQuery("SELECT banner FROM v$version")) {
            rows.next();
            System.out.println(rows.getString(1).contains("compatible with Oracle Database") ? "banner ok" : "banner wrong");
        }
    }

    private static void department(Connection connection) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT first_name, last_name FROM employees WHERE department_id = ? ORDER BY employee_id")) {
            statement.setInt(1, 60);
            try (ResultSet rows = statement.executeQuery()) {
                print(rows, 2);
            }
        }
    }

    private static void join(Connection connection) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT d.department_name, l.city FROM departments d JOIN locations l ON l.location_id = d.location_id "
                        + "WHERE d.department_id = ?")) {
            statement.setInt(1, 10);
            try (ResultSet rows = statement.executeQuery()) {
                print(rows, 2);
            }
        }
    }

    private static void hireDate(Connection connection) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement(
                "SELECT hire_date FROM employees WHERE employee_id = ?")) {
            statement.setInt(1, 100);
            try (ResultSet rows = statement.executeQuery()) {
                rows.next();
                Timestamp hired = rows.getTimestamp(1);
                System.out.println(hired.toLocalDateTime().toLocalDate());
            }
        }
    }

    private static void insertRegion(Connection connection, int id, String name) throws SQLException {
        try (PreparedStatement statement = connection.prepareStatement("INSERT INTO regions VALUES (?, ?)")) {
            statement.setInt(1, id);
            statement.setString(2, name);
            System.out.println("inserted " + statement.executeUpdate());
        }
    }

    private static void transaction(Connection connection) throws SQLException {
        connection.setAutoCommit(false);
        insertRegion(connection, 60, "Antarctica");
        connection.rollback();
        System.out.println("after rollback " + count(connection, "regions"));
        insertRegion(connection, 60, "Antarctica");
        connection.commit();
        connection.setAutoCommit(true);
        System.out.println("after commit " + count(connection, "regions"));
        try (PreparedStatement statement = connection.prepareStatement("DELETE FROM regions WHERE region_id = ?")) {
            statement.setInt(1, 60);
            System.out.println("deleted " + statement.executeUpdate());
        }
        System.out.println("after delete " + count(connection, "regions"));
    }

    private static void missingTable(Connection connection) throws SQLException {
        try (Statement statement = connection.createStatement()) {
            statement.executeQuery("SELECT * FROM no_such_table");
            System.out.println("no error");
        }
    }
}

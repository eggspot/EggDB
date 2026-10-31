import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Savepoint;
import java.sql.Statement;
import java.util.ArrayList;
import java.util.List;

public final class Check {
    public static void main(String[] arguments) throws Exception {
        Path directory = Files.createTempDirectory("eggdb-java-check");
        String url = "jdbc:eggdb:" + directory.resolve("app.db");
        try (Connection connection = DriverManager.getConnection(url)) {
            try (Statement create = connection.createStatement()) {
                create.executeUpdate("CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT, score REAL, photo BLOB)");
            }
            connection.setAutoCommit(false);
            try (PreparedStatement insert = connection.prepareStatement("INSERT INTO people (name, score, photo) VALUES (?, ?, ?)")) {
                for (String name : List.of("Ada", "Grace")) {
                    insert.setString(1, name);
                    insert.setDouble(2, name.length() + 0.5);
                    insert.setBytes(3, new byte[] {0, (byte) 255});
                    insert.addBatch();
                }
                int[] counts = insert.executeBatch();
                check(counts.length == 2 && counts[0] == 1 && counts[1] == 1, "the batch inserted one row per entry");
            }
            connection.commit();
            Savepoint before = connection.setSavepoint();
            try (Statement delete = connection.createStatement()) {
                check(delete.executeUpdate("DELETE FROM people") == 2, "the delete changed two rows");
            }
            connection.rollback(before);
            connection.commit();
            connection.setAutoCommit(true);
            try (PreparedStatement select = connection.prepareStatement("SELECT id, name, score, photo FROM people WHERE score > ? ORDER BY id")) {
                select.setInt(1, 0);
                List<String> rows = new ArrayList<>();
                try (ResultSet result = select.executeQuery()) {
                    check(result.getMetaData().getColumnCount() == 4, "four columns");
                    while (result.next()) {
                        rows.add(result.getLong("id") + " " + result.getString(2) + " " + result.getDouble(3) + " " + result.getBytes(4).length);
                    }
                }
                check(String.join(";", rows).equals("1 Ada 3.5 2;2 Grace 5.5 2"), "rows read back: " + rows);
            }
            try (Statement missing = connection.createStatement()) {
                missing.executeQuery("SELECT * FROM nowhere");
                check(false, "a missing table fails");
            } catch (SQLException error) {
                check(error.getMessage().contains("nowhere"), "the error names the table: " + error.getMessage());
            }
        }
        Files.deleteIfExists(directory.resolve("app.db"));
        Files.deleteIfExists(directory);
        System.out.println("java driver check: passed");
    }

    private static void check(boolean condition, String what) {
        if (!condition) {
            throw new IllegalStateException("java driver check failed: " + what);
        }
    }
}

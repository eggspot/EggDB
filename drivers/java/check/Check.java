import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Savepoint;
import java.sql.Statement;
import java.util.ArrayList;
import java.util.Arrays;
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
            DatabaseMetaData metadata = connection.getMetaData();
            List<String> tables = new ArrayList<>();
            try (ResultSet found = metadata.getTables(null, null, "peo%", new String[] {"TABLE"})) {
                while (found.next()) {
                    tables.add(found.getString("TABLE_NAME") + " " + found.getString("TABLE_TYPE"));
                }
            }
            check(tables.equals(List.of("people TABLE")), "getTables found the table: " + tables);
            List<String> columns = new ArrayList<>();
            try (ResultSet found = metadata.getColumns(null, null, "people", "%")) {
                while (found.next()) {
                    columns.add(found.getString("COLUMN_NAME") + ":" + found.getInt("ORDINAL_POSITION"));
                }
            }
            check(columns.equals(List.of("id:1", "name:2", "score:3", "photo:4")), "getColumns in order: " + columns);
            try (ResultSet keys = metadata.getPrimaryKeys(null, null, "people")) {
                check(keys.next() && keys.getString("COLUMN_NAME").equals("id") && keys.getInt("KEY_SEQ") == 1,
                        "getPrimaryKeys names id");
            }
            try (Statement missing = connection.createStatement()) {
                missing.executeQuery("SELECT * FROM nowhere");
                check(false, "a missing table fails");
            } catch (SQLException error) {
                check(error.getMessage().contains("nowhere"), "the error names the table: " + error.getMessage());
            }
        }
        checkKeyValue(directory.resolve("kv.db"));
        checkCopyAndStepping();
        Files.deleteIfExists(directory.resolve("app.db"));
        Files.deleteIfExists(directory.resolve("kv.db"));
        Files.deleteIfExists(directory);
        System.out.println("java driver check: passed");
    }

    private static void checkCopyAndStepping() throws SQLException {
        try (Connection connection = DriverManager.getConnection("jdbc:eggdb::memory:?dialect=postgresql")) {
            try (Statement create = connection.createStatement()) {
                create.executeUpdate("CREATE TABLE t (id integer, name text)");
            }
            app.eggspot.eggdb.EggDbConnection eggdb = connection.unwrap(app.eggspot.eggdb.EggDbConnection.class);
            long loaded = eggdb.copyFrom("COPY t FROM STDIN",
                    new java.io.ByteArrayInputStream("1\tada\n2\tgrace\n3\tedsger\n".getBytes(java.nio.charset.StandardCharsets.UTF_8)));
            check(loaded == 3, "COPY loads three rows");
            try (PreparedStatement select = connection.prepareStatement("SELECT name FROM t WHERE id > ? ORDER BY id")) {
                select.setLong(1, 1);
                List<String> names = new ArrayList<>();
                try (ResultSet rows = select.executeQuery()) {
                    check(rows.isBeforeFirst(), "the stepped result set has rows");
                    while (rows.next()) {
                        names.add(rows.getString(1));
                    }
                    check(rows.isAfterLast(), "the stepped result set ends");
                }
                check(names.equals(List.of("grace", "edsger")), "stepped rows: " + names);
            }
            java.io.ByteArrayOutputStream copied = new java.io.ByteArrayOutputStream();
            eggdb.copyTo("COPY (SELECT id FROM t ORDER BY id) TO STDOUT", copied);
            check(copied.toString(java.nio.charset.StandardCharsets.UTF_8).equals("1\n2\n3\n"), "COPY reads the rows back");
        }
    }

    private static void checkKeyValue(Path file) throws SQLException {
        try (EggDbKeyValue store = EggDbKeyValue.open(file, false)) {
            store.write(txn -> {
                txn.put(null, EggDbTransaction.bytes("a"), EggDbTransaction.bytes("1"));
                txn.put(null, EggDbTransaction.bytes("b"), new byte[] {0, 2});
                check(txn.createCollection("users"), "a collection is created");
                txn.put("users", EggDbTransaction.bytes("u1"), EggDbTransaction.bytes("Ada"));
                return null;
            });
            store.read(txn -> {
                check(Arrays.equals(txn.get(null, EggDbTransaction.bytes("a")), EggDbTransaction.bytes("1")), "a stored value");
                check(txn.get(null, EggDbTransaction.bytes("missing")) == null, "a missing key");
                List<EggDbTransaction.Entry> all = txn.scan(null, EggDbTransaction.Range.all());
                check(all.size() == 2 && Arrays.equals(all.get(1).value(), new byte[] {0, 2}), "a scan with binary values");
                List<EggDbTransaction.Entry> reversed = txn.scan(null, EggDbTransaction.Range.all().reversed());
                check(Arrays.equals(reversed.get(0).key(), EggDbTransaction.bytes("b")), "a reversed scan");
                check(txn.collections().equals(List.of("users")), "the collection names");
                check(txn.collectionStats("users").entries() == 1, "the collection stats");
                return null;
            });
            store.write(txn -> {
                check(txn.delete(null, EggDbTransaction.bytes("a")), "a key is deleted");
                txn.renameCollection("users", "people");
                return null;
            });
            store.read(txn -> {
                check(txn.get(null, EggDbTransaction.bytes("a")) == null, "the deleted key is gone");
                check(Arrays.equals(txn.get("people", EggDbTransaction.bytes("u1")), EggDbTransaction.bytes("Ada")),
                        "the renamed collection keeps its keys");
                return null;
            });
        }
    }

    private static void check(boolean condition, String what) {
        if (!condition) {
            throw new IllegalStateException("java driver check failed: " + what);
        }
    }
}

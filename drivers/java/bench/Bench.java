import java.nio.file.Files;
import java.nio.file.Path;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLException;
import java.sql.Statement;

public final class Bench {
    private static final int ROWS = Integer.parseInt(System.getenv().getOrDefault("DRIVER_BENCH_ROWS", "20000"));
    private static final int LOOKUPS = Integer.parseInt(System.getenv().getOrDefault("DRIVER_BENCH_LOOKUPS", "5000"));

    private interface Work {
        void run() throws SQLException;
    }

    private static double timed(Work work) throws SQLException {
        long started = System.nanoTime();
        work.run();
        return (System.nanoTime() - started) / 1e6;
    }

    private static double[] side(String url) throws SQLException {
        try (Connection connection = DriverManager.getConnection(url)) {
            try (Statement create = connection.createStatement()) {
                create.executeUpdate("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL)");
            }
            double insert = timed(() -> {
                connection.setAutoCommit(false);
                try (PreparedStatement statement = connection.prepareStatement("INSERT INTO t VALUES (?, ?, ?)")) {
                    for (int at = 1; at <= ROWS; at++) {
                        statement.setLong(1, at);
                        statement.setString(2, "name " + at);
                        statement.setDouble(3, at * 0.25);
                        statement.executeUpdate();
                    }
                }
                connection.commit();
                connection.setAutoCommit(true);
            });
            double lookup = timed(() -> {
                try (PreparedStatement statement = connection.prepareStatement("SELECT name FROM t WHERE id = ?")) {
                    for (int at = 1; at <= LOOKUPS; at++) {
                        statement.setLong(1, at * 7L % ROWS + 1);
                        try (ResultSet rows = statement.executeQuery()) {
                            while (rows.next()) {
                                rows.getString(1);
                            }
                        }
                    }
                }
            });
            double scan = timed(() -> {
                try (PreparedStatement statement = connection.prepareStatement("SELECT name FROM t");
                        ResultSet rows = statement.executeQuery()) {
                    long total = 0;
                    while (rows.next()) {
                        total += rows.getString(1).length();
                    }
                    if (total < 0) {
                        throw new SQLException("impossible");
                    }
                }
            });
            return new double[] {insert, lookup, scan};
        }
    }

    public static void main(String[] arguments) throws Exception {
        Path directory = Files.createTempDirectory("eggdb-java-bench");
        double[] egg = side("jdbc:eggdb:" + directory.resolve("bench.eggdb"));
        double[] lite = side("jdbc:sqlite:" + directory.resolve("bench.sqlite"));
        String[] workloads = {"insert", "lookup", "scan"};
        for (int at = 0; at < workloads.length; at++) {
            System.out.printf("java %s %.2f %.2f%n", workloads[at], egg[at], lite[at]);
        }
    }
}

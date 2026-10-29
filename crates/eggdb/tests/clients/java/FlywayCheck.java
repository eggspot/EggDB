import java.nio.file.Path;
import java.sql.Connection;
import java.sql.DriverManager;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Statement;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import org.flywaydb.core.Flyway;
import org.flywaydb.core.api.FlywayException;
import org.flywaydb.core.api.MigrationInfo;
import org.flywaydb.core.api.logging.Log;
import org.flywaydb.core.api.logging.LogFactory;
import org.flywaydb.core.api.output.MigrateOutput;
import org.flywaydb.core.api.output.MigrateResult;
import org.flywaydb.core.api.output.ValidateResult;

public final class FlywayCheck {
    private interface Step {
        void run() throws SQLException;
    }

    private static final class StandardErrorLog implements Log {
        private final String source;

        StandardErrorLog(Class<?> origin) {
            source = origin.getSimpleName();
        }

        @Override
        public boolean isDebugEnabled() {
            return false;
        }

        @Override
        public void debug(String message) {
        }

        @Override
        public void info(String message) {
            write("INFO", message);
        }

        @Override
        public void warn(String message) {
            write("WARN", message);
        }

        @Override
        public void error(String message) {
            write("ERROR", message);
        }

        @Override
        public void error(String message, Exception error) {
            write("ERROR", message + ": " + error);
        }

        @Override
        public void notice(String message) {
            write("NOTICE", message);
        }

        private void write(String level, String message) {
            System.err.println("flyway " + level + " " + source + " " + message);
        }
    }

    private final String url;
    private final String user;
    private final String password;
    private final Path migrations;
    private final Map<String, String> settings;

    private FlywayCheck(String url, String user, String password, Path migrations, Map<String, String> settings) {
        this.url = url;
        this.user = user;
        this.password = password;
        this.migrations = migrations;
        this.settings = settings;
    }

    public static void main(String[] arguments) {
        boolean sessionLock = arguments.length == 5 && arguments[4].equals("session-lock");
        if (arguments.length != 4 && !sessionLock) {
            System.err.println("usage: java -cp <flyway and pgjdbc jars> FlywayCheck.java <jdbc url> <user> <password> <migrations directory> [session-lock]");
            System.exit(2);
        }
        LogFactory.setFallbackLogCreator(StandardErrorLog::new);
        Map<String, String> settings = sessionLock ? Map.of("flyway.postgresql.transactional.lock", "false") : Map.of();
        FlywayCheck check = new FlywayCheck(arguments[0], arguments[1], arguments[2], Path.of(arguments[3]).toAbsolutePath(), settings);
        Flyway base = check.flyway("base");
        Flyway later = check.flyway("base", "later");
        step("migrate", () -> printMigrate(base.migrate()));
        step("info", () -> printInfo(base.info().all()));
        step("validate", () -> printValidate(base.validateWithResult()));
        step("migrate again", () -> printMigrate(base.migrate()));
        step("pending", () -> {
            printInfo(later.info().all());
            printMigrate(later.migrate());
            printInfo(later.info().all());
            printValidate(later.validateWithResult());
        });
        step("rows", check::rows);
        step("history", check::history);
    }

    private Flyway flyway(String... folders) {
        String[] locations = new String[folders.length];
        for (int at = 0; at < folders.length; at++) {
            locations[at] = "filesystem:" + migrations.resolve(folders[at]);
        }
        return Flyway.configure()
                .configuration(settings)
                .dataSource(url, user, password)
                .locations(locations)
                .cleanDisabled(true)
                .loggers("console")
                .load();
    }

    private static void step(String name, Step body) {
        System.out.println("== " + name);
        try {
            body.run();
        } catch (FlywayException error) {
            System.out.println("flyway error " + error.getClass().getSimpleName() + " " + error.getErrorCode());
            printSqlState(error);
        } catch (SQLException error) {
            System.out.println("error " + error.getSQLState());
        }
    }

    private static void printSqlState(Throwable error) {
        for (Throwable cause = error; cause != null; cause = cause.getCause()) {
            if (cause instanceof SQLException failure) {
                System.out.println("error " + failure.getSQLState());
                return;
            }
        }
    }

    private static String shown(Object value) {
        return value == null ? "NULL" : value.toString();
    }

    private static void printMigrate(MigrateResult result) {
        System.out.println("executed " + result.migrationsExecuted);
        System.out.println("target " + shown(result.targetSchemaVersion));
        System.out.println("success " + result.success);
        for (MigrateOutput migration : result.migrations) {
            System.out.println(String.join("\t", shown(migration.category), shown(migration.version),
                    shown(migration.description), shown(migration.type), Path.of(migration.filepath).getFileName().toString()));
        }
    }

    private static void printInfo(MigrationInfo[] infos) {
        for (MigrationInfo info : infos) {
            System.out.println(String.join("\t", shown(info.getVersion()), shown(info.getDescription()),
                    shown(info.getType()), shown(info.getScript()), shown(info.getChecksum()),
                    info.getState().name(), String.valueOf(info.getState().isApplied())));
        }
    }

    private static void printValidate(ValidateResult result) {
        System.out.println("valid " + result.validationSuccessful);
        System.out.println("invalid " + result.invalidMigrations.size());
        System.out.println("error " + (result.errorDetails == null ? "NULL" : shown(result.errorDetails.errorCode)));
    }

    private Connection connect() throws SQLException {
        return DriverManager.getConnection(url, user, password);
    }

    private void rows() throws SQLException {
        try (Connection connection = connect(); Statement statement = connection.createStatement()) {
            query(statement, "SELECT id, email, name FROM customers ORDER BY id");
            query(statement, "SELECT id, customer_id, total, placed_on, status FROM orders ORDER BY id");
            query(statement, "SELECT id, name, order_count, spent FROM customer_totals ORDER BY id");
        }
    }

    private void history() throws SQLException {
        try (Connection connection = connect(); Statement statement = connection.createStatement()) {
            query(statement, "SELECT installed_rank, version, description, type, script, checksum, success "
                    + "FROM flyway_schema_history ORDER BY installed_rank");
        }
    }

    private static void query(Statement statement, String sql) throws SQLException {
        try (ResultSet rows = statement.executeQuery(sql)) {
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
    }
}

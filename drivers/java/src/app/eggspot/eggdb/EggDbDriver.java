package app.eggspot.eggdb;

import java.sql.Connection;
import java.sql.Driver;
import java.sql.DriverManager;
import java.sql.DriverPropertyInfo;
import java.sql.SQLException;
import java.sql.SQLFeatureNotSupportedException;
import java.util.Locale;
import java.util.Properties;
import java.util.logging.Logger;

/**
 * EggDB's JDBC driver, embedded in the process through EggDB's C API.
 *
 * <p>URLs are {@code jdbc:eggdb:<path>} with optional {@code ?dialect=<name>&mode=ro};
 * {@code jdbc:eggdb::memory:} opens a database in memory. The dialect is {@code sqlite} unless
 * given. The C library is found through the {@code eggdb.library} system property, the
 * {@code EGGDB_LIBRARY} environment variable or the system's library path.
 */
public final class EggDbDriver implements Driver {
    /** The prefix of every URL this driver accepts. */
    public static final String URL_PREFIX = "jdbc:eggdb:";

    static {
        try {
            DriverManager.registerDriver(new EggDbDriver());
        } catch (SQLException error) {
            throw new ExceptionInInitializerError(error);
        }
    }

    @Override
    public Connection connect(String url, Properties info) throws SQLException {
        if (!acceptsURL(url)) {
            return null;
        }
        String rest = url.substring(URL_PREFIX.length());
        Properties settings = new Properties();
        if (info != null) {
            settings.putAll(info);
        }
        int query = rest.indexOf('?');
        String path = query < 0 ? rest : rest.substring(0, query);
        if (query >= 0) {
            for (String pair : rest.substring(query + 1).split("&")) {
                int equals = pair.indexOf('=');
                if (equals > 0) {
                    settings.setProperty(pair.substring(0, equals), pair.substring(equals + 1));
                }
            }
        }
        String dialect = settings.getProperty("dialect", "sqlite").toLowerCase(Locale.ROOT);
        if (!java.util.Set.of("sqlite", "postgresql", "mysql", "oracle", "sqlserver").contains(dialect)) {
            throw new SQLException("unknown dialect " + dialect, "08001");
        }
        boolean readOnly = settings.getProperty("mode", "").equalsIgnoreCase("ro");
        return new EggDbConnection(new NativeSession(path, dialect, readOnly), url, readOnly);
    }

    @Override
    public boolean acceptsURL(String url) {
        return url != null && url.startsWith(URL_PREFIX);
    }

    @Override
    public DriverPropertyInfo[] getPropertyInfo(String url, Properties info) {
        DriverPropertyInfo dialect = new DriverPropertyInfo("dialect", "sqlite");
        dialect.choices = new String[] {"sqlite", "postgresql", "mysql", "oracle", "sqlserver"};
        DriverPropertyInfo mode = new DriverPropertyInfo("mode", "");
        mode.choices = new String[] {"", "ro"};
        return new DriverPropertyInfo[] {dialect, mode};
    }

    @Override
    public int getMajorVersion() {
        return 0;
    }

    @Override
    public int getMinorVersion() {
        return 1;
    }

    @Override
    public boolean jdbcCompliant() {
        return false;
    }

    @Override
    public Logger getParentLogger() throws SQLFeatureNotSupportedException {
        throw new SQLFeatureNotSupportedException("EggDB's driver does not log through java.util.logging");
    }
}

package app.eggspot.eggdb;

import java.io.InputStream;
import java.io.OutputStream;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

final class NativeSession implements AutoCloseable {
    record StatementResult(String tag, List<String> columns, List<Object[]> rows) {
        boolean returnsRows() {
            return !columns.isEmpty();
        }

        int changedRows() {
            String[] words = tag.split(" ");
            if (words.length > 1 && (words[0].equals("INSERT") || words[0].equals("UPDATE") || words[0].equals("DELETE"))) {
                try {
                    return Integer.parseInt(words[words.length - 1]);
                } catch (NumberFormatException notCounted) {
                    return 0;
                }
            }
            return 0;
        }
    }

    private static final long CLOSED = 0;

    private final NativeCalls calls = NativeCalls.get();
    private final String dialect;
    private final Set<NativeStatement> statements = new HashSet<>();
    private long database;
    private long session;
    private boolean ownsDatabase = true;

    static boolean isServerUrl(String path) {
        return path.startsWith("postgresql://") || path.startsWith("postgres://");
    }

    NativeSession(String path, String dialect, boolean readOnly) throws SQLException {
        this.dialect = isServerUrl(path) ? "postgresql" : dialect;
        if (isServerUrl(path)) {
            session = calls.connect(path);
            return;
        }
        database = calls.open(path, readOnly);
        try {
            session = calls.sqlOpen(database, dialect);
        } catch (SQLException error) {
            calls.close(database);
            database = CLOSED;
            throw error;
        }
    }

    private NativeSession(String dialect, long database, long session) {
        this.dialect = dialect;
        this.database = database;
        this.session = session;
        this.ownsDatabase = false;
    }

    String dialect() {
        return dialect;
    }

    boolean speaksPostgresql() {
        return dialect.equals("postgresql");
    }

    private long openSession() throws SQLException {
        if (session == CLOSED) {
            throw new SQLException("the connection is closed", "08003");
        }
        return session;
    }

    NativeSession catalogSession() throws SQLException {
        openSession();
        if (speaksPostgresql() || database == CLOSED) {
            return this;
        }
        return new NativeSession("postgresql", database, calls.sqlOpen(database, "postgresql"));
    }

    boolean isClosed() {
        return session == CLOSED;
    }

    List<StatementResult> execute(String sql, List<Object> parameters) throws SQLException {
        long open = openSession();
        Parameters.Bound bound = Parameters.bound(sql, dialect, parameters);
        List<String> texts = new ArrayList<>(bound.parameters().size());
        for (Object value : bound.parameters()) {
            texts.add(Parameters.text(value, dialect));
        }
        return calls.execute(open, bound.sql(), texts);
    }

    NativeStatement prepare(String sql) throws SQLException {
        NativeStatement statement = new NativeStatement(calls, openSession(), Parameters.bound(sql, dialect, List.of()).sql(),
                dialect, statements::remove);
        statements.add(statement);
        return statement;
    }

    long copyIn(String sql, InputStream source) throws SQLException {
        return calls.copyIn(openSession(), sql, source);
    }

    void copyOut(String sql, OutputStream sink) throws SQLException {
        calls.copyOut(openSession(), sql, sink);
    }

    static long copiedRows(List<StatementResult> results) {
        for (int index = results.size() - 1; index >= 0; index--) {
            String[] words = results.get(index).tag().split(" ");
            if (words.length == 2 && words[0].equals("COPY")) {
                try {
                    return Long.parseLong(words[1]);
                } catch (NumberFormatException notCounted) {
                    return -1;
                }
            }
        }
        return -1;
    }

    @Override
    public void close() {
        if (session == CLOSED) {
            return;
        }
        for (NativeStatement statement : List.copyOf(statements)) {
            statement.close();
        }
        calls.sqlClose(session);
        if (database != CLOSED && ownsDatabase) {
            calls.close(database);
        }
        session = CLOSED;
        database = CLOSED;
    }
}

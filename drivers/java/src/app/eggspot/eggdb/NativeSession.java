package app.eggspot.eggdb;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.List;

final class NativeSession implements AutoCloseable {
    private static final int NULL = 0;
    private static final int INTEGER = 1;
    private static final int REAL = 2;
    private static final int BLOB = 4;
    private static final int OPEN_READ_ONLY = 1;
    static final long VALUE_BYTES = 40;
    private static final long PARAMETER_BYTES = 16;
    private static final long RESULT_BYTES = 40;

    private static final class Api {
        static final MethodHandle OPEN;
        static final MethodHandle CLOSE;
        static final MethodHandle SQL_OPEN;
        static final MethodHandle SQL_CLOSE;
        static final MethodHandle SQL_CONNECT;
        static final MethodHandle EXECUTE;
        static final MethodHandle LAST_ERROR;
        static final MethodHandle LAST_SQLSTATE;
        static final MethodHandle COPY_IN_BEGIN;
        static final MethodHandle COPY_OUT_BEGIN;
        static final MethodHandle COPY_PUT;
        static final MethodHandle COPY_READ;
        static final MethodHandle COPY_END;
        static final MethodHandle COPY_FREE;

        static {
            ValueLayout pointer = ValueLayout.ADDRESS;
            ValueLayout status = ValueLayout.JAVA_INT;
            OPEN = NativeLibrary.handle("eggdb_open", FunctionDescriptor.of(status, pointer, pointer, pointer));
            CLOSE = NativeLibrary.handle("eggdb_close", FunctionDescriptor.of(status, pointer));
            SQL_OPEN = NativeLibrary.handle("eggdb_sql_open", FunctionDescriptor.of(status, pointer, pointer, pointer));
            SQL_CLOSE = NativeLibrary.handle("eggdb_sql_close", FunctionDescriptor.of(status, pointer));
            SQL_CONNECT = NativeLibrary.handle("eggdb_sql_connect", FunctionDescriptor.of(status, pointer, pointer));
            EXECUTE = NativeLibrary.handle("eggdb_sql_execute_with_parameters",
                    FunctionDescriptor.of(status, pointer, pointer, pointer, ValueLayout.JAVA_LONG, pointer, pointer));
            LAST_ERROR = NativeLibrary.handle("eggdb_last_error", FunctionDescriptor.of(pointer));
            LAST_SQLSTATE = NativeLibrary.handle("eggdb_last_sqlstate", FunctionDescriptor.of(pointer));
            COPY_IN_BEGIN = NativeLibrary.handle("eggdb_sql_copy_in_begin", FunctionDescriptor.of(status, pointer, pointer, pointer));
            COPY_OUT_BEGIN = NativeLibrary.handle("eggdb_sql_copy_out_begin", FunctionDescriptor.of(status, pointer, pointer, pointer));
            COPY_PUT = NativeLibrary.handle("eggdb_copy_put", FunctionDescriptor.of(status, pointer, pointer, ValueLayout.JAVA_LONG));
            COPY_READ = NativeLibrary.handle("eggdb_copy_read", FunctionDescriptor.of(status, pointer, pointer, pointer));
            COPY_END = NativeLibrary.handle("eggdb_copy_end", FunctionDescriptor.of(status, pointer, pointer, pointer));
            COPY_FREE = NativeLibrary.handle("eggdb_copy_free", FunctionDescriptor.ofVoid(pointer));
        }
    }

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

    private final String dialect;
    private MemorySegment database;
    private MemorySegment session;
    private boolean ownsDatabase = true;

    static boolean isServerUrl(String path) {
        return path.startsWith("postgresql://") || path.startsWith("postgres://");
    }

    NativeSession(String path, String dialect, boolean readOnly) throws SQLException {
        this.dialect = isServerUrl(path) ? "postgresql" : dialect;
        try (Arena arena = Arena.ofConfined()) {
            if (isServerUrl(path)) {
                MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
                check((int) Api.SQL_CONNECT.invokeExact(arena.allocateFrom(path), out));
                database = null;
                session = out.get(ValueLayout.ADDRESS, 0);
                return;
            }
            MemorySegment options = arena.allocate(16, 8);
            options.set(ValueLayout.JAVA_INT, 0, readOnly ? OPEN_READ_ONLY : 0);
            MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
            check((int) Api.OPEN.invokeExact(arena.allocateFrom(path), options, out));
            database = out.get(ValueLayout.ADDRESS, 0);
            int status = (int) Api.SQL_OPEN.invokeExact(database, arena.allocateFrom(dialect), out);
            if (status != 0) {
                SQLException error = lastError(status);
                int ignored = (int) Api.CLOSE.invokeExact(database);
                database = null;
                throw error;
            }
            session = out.get(ValueLayout.ADDRESS, 0);
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw new SQLException("EggDB's C library could not be called: " + error, "58000", error);
        }
    }

    private NativeSession(String dialect, MemorySegment database, MemorySegment session) {
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

    NativeSession catalogSession() throws SQLException {
        if (session == null) {
            throw new SQLException("the connection is closed", "08003");
        }
        if (speaksPostgresql() || database == null) {
            return this;
        }
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
            check((int) Api.SQL_OPEN.invokeExact(database, arena.allocateFrom("postgresql"), out));
            return new NativeSession("postgresql", database, out.get(ValueLayout.ADDRESS, 0));
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw new SQLException("EggDB's C library could not be called: " + error, "58000", error);
        }
    }

    boolean isClosed() {
        return session == null;
    }

    List<StatementResult> execute(String sql, List<Object> parameters) throws SQLException {
        if (session == null) {
            throw new SQLException("the connection is closed", "08003");
        }
        Parameters.Bound bound = Parameters.bound(sql, dialect, parameters);
        parameters = bound.parameters();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment array = arena.allocate(PARAMETER_BYTES * Math.max(parameters.size(), 1), 8);
            for (int index = 0; index < parameters.size(); index++) {
                String text = Parameters.text(parameters.get(index), dialect);
                if (text != null) {
                    byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
                    MemorySegment copy = arena.allocate(bytes.length + 1L);
                    MemorySegment.copy(bytes, 0, copy, ValueLayout.JAVA_BYTE, 0, bytes.length);
                    array.set(ValueLayout.ADDRESS, index * PARAMETER_BYTES, copy);
                    array.set(ValueLayout.JAVA_LONG, index * PARAMETER_BYTES + 8, bytes.length);
                }
            }
            MemorySegment results = arena.allocate(ValueLayout.ADDRESS);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) Api.EXECUTE.invokeExact(session, arena.allocateFrom(bound.sql()), array,
                    (long) parameters.size(), results, count);
            if (status != 0) {
                throw lastError(status);
            }
            return gathered(results.get(ValueLayout.ADDRESS, 0), count.get(ValueLayout.JAVA_LONG, 0));
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw new SQLException("EggDB's C library could not be called: " + error, "58000", error);
        }
    }

    static List<StatementResult> gathered(MemorySegment first, long count) {
        List<StatementResult> gathered = new ArrayList<>();
        MemorySegment results = first.reinterpret(RESULT_BYTES * count);
        for (long index = 0; index < count; index++) {
            long at = index * RESULT_BYTES;
            String tag = text(results.get(ValueLayout.ADDRESS, at));
            long columnCount = results.get(ValueLayout.JAVA_LONG, at + 8);
            MemorySegment names = results.get(ValueLayout.ADDRESS, at + 16).reinterpret(8 * columnCount);
            long rowCount = results.get(ValueLayout.JAVA_LONG, at + 24);
            MemorySegment values = results.get(ValueLayout.ADDRESS, at + 32).reinterpret(VALUE_BYTES * columnCount * rowCount);
            List<String> columns = new ArrayList<>();
            for (long column = 0; column < columnCount; column++) {
                columns.add(text(names.get(ValueLayout.ADDRESS, 8 * column)));
            }
            List<Object[]> rows = new ArrayList<>();
            for (long row = 0; row < rowCount; row++) {
                Object[] cells = new Object[(int) columnCount];
                for (int column = 0; column < columnCount; column++) {
                    cells[column] = converted(values, VALUE_BYTES * (row * columnCount + column));
                }
                rows.add(cells);
            }
            gathered.add(new StatementResult(tag, columns, rows));
        }
        return gathered;
    }

    static Object converted(MemorySegment values, long at) {
        int kind = values.get(ValueLayout.JAVA_INT, at);
        if (kind == NULL) {
            return null;
        }
        if (kind == INTEGER) {
            return values.get(ValueLayout.JAVA_LONG, at + 8);
        }
        if (kind == REAL) {
            return values.get(ValueLayout.JAVA_DOUBLE, at + 16);
        }
        long length = values.get(ValueLayout.JAVA_LONG, at + 32);
        byte[] bytes = values.get(ValueLayout.ADDRESS, at + 24).reinterpret(length).toArray(ValueLayout.JAVA_BYTE);
        return kind == BLOB ? bytes : new String(bytes, StandardCharsets.UTF_8);
    }

    static String text(MemorySegment pointer) {
        return pointer.equals(MemorySegment.NULL) ? "" : pointer.reinterpret(Long.MAX_VALUE).getString(0);
    }

    static SQLException callFailed(Throwable error) {
        return new SQLException("EggDB's C library could not be called: " + error, "58000", error);
    }

    private MemorySegment openSession() throws SQLException {
        if (session == null) {
            throw new SQLException("the connection is closed", "08003");
        }
        return session;
    }

    private final java.util.Set<NativeStatement> statements = new java.util.HashSet<>();

    NativeStatement prepare(String sql) throws SQLException {
        NativeStatement statement = new NativeStatement(openSession(), Parameters.bound(sql, dialect, List.of()).sql(), dialect,
                statements::remove);
        statements.add(statement);
        return statement;
    }

    long copyIn(String sql, java.io.InputStream source) throws SQLException {
        MemorySegment open = openSession();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
            check((int) Api.COPY_IN_BEGIN.invokeExact(open, arena.allocateFrom(sql), out));
            MemorySegment copy = out.get(ValueLayout.ADDRESS, 0);
            try {
                byte[] buffer = new byte[64 * 1024];
                MemorySegment piece = arena.allocate(buffer.length);
                int read;
                while ((read = source.read(buffer)) > 0) {
                    MemorySegment.copy(buffer, 0, piece, ValueLayout.JAVA_BYTE, 0, read);
                    check((int) Api.COPY_PUT.invokeExact(copy, piece, (long) read));
                }
                return copiedRows(endCopy(arena, copy));
            } finally {
                Api.COPY_FREE.invokeExact(copy);
            }
        } catch (SQLException error) {
            throw error;
        } catch (java.io.IOException error) {
            throw new SQLException("reading the COPY data failed: " + error.getMessage(), "58030", error);
        } catch (Throwable error) {
            throw callFailed(error);
        }
    }

    void copyOut(String sql, java.io.OutputStream sink) throws SQLException {
        MemorySegment open = openSession();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
            check((int) Api.COPY_OUT_BEGIN.invokeExact(open, arena.allocateFrom(sql), out));
            MemorySegment copy = out.get(ValueLayout.ADDRESS, 0);
            try {
                MemorySegment data = arena.allocate(ValueLayout.ADDRESS);
                MemorySegment length = arena.allocate(ValueLayout.JAVA_LONG);
                while (true) {
                    check((int) Api.COPY_READ.invokeExact(copy, data, length));
                    long size = length.get(ValueLayout.JAVA_LONG, 0);
                    if (size == 0) {
                        break;
                    }
                    sink.write(data.get(ValueLayout.ADDRESS, 0).reinterpret(size).toArray(ValueLayout.JAVA_BYTE));
                }
                endCopy(arena, copy);
            } finally {
                Api.COPY_FREE.invokeExact(copy);
            }
        } catch (SQLException error) {
            throw error;
        } catch (java.io.IOException error) {
            throw new SQLException("writing the COPY data failed: " + error.getMessage(), "58030", error);
        } catch (Throwable error) {
            throw callFailed(error);
        }
    }

    private static List<StatementResult> endCopy(Arena arena, MemorySegment copy) throws Throwable {
        MemorySegment results = arena.allocate(ValueLayout.ADDRESS);
        MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
        check((int) Api.COPY_END.invokeExact(copy, results, count));
        return gathered(results.get(ValueLayout.ADDRESS, 0), count.get(ValueLayout.JAVA_LONG, 0));
    }

    private static long copiedRows(List<StatementResult> results) {
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

    static SQLException lastError(int status) {
        try {
            String message = text((MemorySegment) Api.LAST_ERROR.invokeExact());
            String sqlState = text((MemorySegment) Api.LAST_SQLSTATE.invokeExact());
            return new SQLException(message.isEmpty() ? "EggDB status " + status : message,
                    sqlState.isEmpty() ? null : sqlState, status);
        } catch (Throwable error) {
            return new SQLException("EggDB status " + status, null, status, error);
        }
    }

    @Override
    public void close() {
        if (session == null) {
            return;
        }
        for (NativeStatement statement : List.copyOf(statements)) {
            statement.close();
        }
        try {
            int sessionClosed = (int) Api.SQL_CLOSE.invokeExact(session);
            if (database != null && ownsDatabase) {
                int databaseClosed = (int) Api.CLOSE.invokeExact(database);
            }
        } catch (Throwable ignored) {
            return;
        } finally {
            session = null;
            database = null;
        }
    }

    private static void check(int status) throws SQLException {
        if (status != 0) {
            throw lastError(status);
        }
    }
}

package app.eggspot.eggdb;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.nio.file.Path;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.List;

final class NativeSession implements AutoCloseable {
    private static final int NULL = 0;
    private static final int INTEGER = 1;
    private static final int REAL = 2;
    private static final int BLOB = 4;
    private static final int OPEN_READ_ONLY = 1;
    private static final long VALUE_BYTES = 40;
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

        static {
            Linker linker = Linker.nativeLinker();
            SymbolLookup library = library();
            ValueLayout pointer = ValueLayout.ADDRESS;
            ValueLayout status = ValueLayout.JAVA_INT;
            OPEN = handle(linker, library, "eggdb_open", FunctionDescriptor.of(status, pointer, pointer, pointer));
            CLOSE = handle(linker, library, "eggdb_close", FunctionDescriptor.of(status, pointer));
            SQL_OPEN = handle(linker, library, "eggdb_sql_open", FunctionDescriptor.of(status, pointer, pointer, pointer));
            SQL_CLOSE = handle(linker, library, "eggdb_sql_close", FunctionDescriptor.of(status, pointer));
            SQL_CONNECT = handle(linker, library, "eggdb_sql_connect", FunctionDescriptor.of(status, pointer, pointer));
            EXECUTE = handle(linker, library, "eggdb_sql_execute_with_parameters",
                    FunctionDescriptor.of(status, pointer, pointer, pointer, ValueLayout.JAVA_LONG, pointer, pointer));
            LAST_ERROR = handle(linker, library, "eggdb_last_error", FunctionDescriptor.of(pointer));
            LAST_SQLSTATE = handle(linker, library, "eggdb_last_sqlstate", FunctionDescriptor.of(pointer));
        }

        private static SymbolLookup library() {
            String given = System.getProperty("eggdb.library", System.getenv("EGGDB_LIBRARY"));
            if (given != null && !given.isEmpty()) {
                return SymbolLookup.libraryLookup(Path.of(given), Arena.global());
            }
            return SymbolLookup.libraryLookup(System.mapLibraryName("eggdb_capi"), Arena.global());
        }

        private static MethodHandle handle(Linker linker, SymbolLookup library, String name, FunctionDescriptor descriptor) {
            MemorySegment symbol = library.find(name)
                    .orElseThrow(() -> new UnsatisfiedLinkError("the EggDB C library has no " + name));
            return linker.downcallHandle(symbol, descriptor);
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

    String dialect() {
        return dialect;
    }

    boolean isClosed() {
        return session == null;
    }

    List<StatementResult> execute(String sql, List<Object> parameters) throws SQLException {
        if (session == null) {
            throw new SQLException("the connection is closed", "08003");
        }
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
            String numbered = Parameters.numbered(sql, dialect);
            int status = (int) Api.EXECUTE.invokeExact(session, arena.allocateFrom(numbered), array,
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

    private static List<StatementResult> gathered(MemorySegment first, long count) {
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

    private static Object converted(MemorySegment values, long at) {
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

    private static String text(MemorySegment pointer) {
        return pointer.equals(MemorySegment.NULL) ? "" : pointer.reinterpret(Long.MAX_VALUE).getString(0);
    }

    private static SQLException lastError(int status) {
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
        try {
            int sessionClosed = (int) Api.SQL_CLOSE.invokeExact(session);
            if (database != null) {
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

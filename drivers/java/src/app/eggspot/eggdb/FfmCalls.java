package app.eggspot.eggdb;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.nio.charset.StandardCharsets;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.List;

final class FfmCalls implements NativeCalls {
    private static final int OPEN_READ_ONLY = 1;
    private static final long VALUE_BYTES = 40;
    private static final long PARAMETER_BYTES = 16;
    private static final long RESULT_BYTES = 40;
    private static final int COPY_PIECE_BYTES = 64 * 1024;

    private static final class Api {
        static final MethodHandle OPEN;
        static final MethodHandle CLOSE;
        static final MethodHandle SQL_OPEN;
        static final MethodHandle SQL_CLOSE;
        static final MethodHandle SQL_CONNECT;
        static final MethodHandle EXECUTE;
        static final MethodHandle LAST_ERROR;
        static final MethodHandle LAST_SQLSTATE;
        static final MethodHandle PREPARE;
        static final MethodHandle PARAMETER_COUNT;
        static final MethodHandle BIND_NULL;
        static final MethodHandle BIND_INT64;
        static final MethodHandle BIND_DOUBLE;
        static final MethodHandle BIND_BOOL;
        static final MethodHandle BIND_TEXT;
        static final MethodHandle BIND_BLOB;
        static final MethodHandle CLEAR_BINDINGS;
        static final MethodHandle STATEMENT_EXECUTE;
        static final MethodHandle RESET;
        static final MethodHandle COLUMN_COUNT;
        static final MethodHandle COLUMN_NAME;
        static final MethodHandle STEP;
        static final MethodHandle ROW;
        static final MethodHandle FINALIZE;
        static final MethodHandle COPY_IN_BEGIN;
        static final MethodHandle COPY_OUT_BEGIN;
        static final MethodHandle COPY_PUT;
        static final MethodHandle COPY_READ;
        static final MethodHandle COPY_END;
        static final MethodHandle COPY_FREE;

        static {
            ValueLayout pointer = ValueLayout.ADDRESS;
            ValueLayout status = ValueLayout.JAVA_INT;
            ValueLayout size = ValueLayout.JAVA_LONG;
            OPEN = NativeLibrary.handle("eggdb_open", FunctionDescriptor.of(status, pointer, pointer, pointer));
            CLOSE = NativeLibrary.handle("eggdb_close", FunctionDescriptor.of(status, pointer));
            SQL_OPEN = NativeLibrary.handle("eggdb_sql_open", FunctionDescriptor.of(status, pointer, pointer, pointer));
            SQL_CLOSE = NativeLibrary.handle("eggdb_sql_close", FunctionDescriptor.of(status, pointer));
            SQL_CONNECT = NativeLibrary.handle("eggdb_sql_connect", FunctionDescriptor.of(status, pointer, pointer));
            EXECUTE = NativeLibrary.handle("eggdb_sql_execute_with_parameters",
                    FunctionDescriptor.of(status, pointer, pointer, pointer, size, pointer, pointer));
            LAST_ERROR = NativeLibrary.handle("eggdb_last_error", FunctionDescriptor.of(pointer));
            LAST_SQLSTATE = NativeLibrary.handle("eggdb_last_sqlstate", FunctionDescriptor.of(pointer));
            PREPARE = NativeLibrary.handle("eggdb_sql_prepare", FunctionDescriptor.of(status, pointer, pointer, size, pointer));
            PARAMETER_COUNT = NativeLibrary.handle("eggdb_stmt_parameter_count", FunctionDescriptor.of(status, pointer, pointer));
            BIND_NULL = NativeLibrary.handle("eggdb_stmt_bind_null", FunctionDescriptor.of(status, pointer, size));
            BIND_INT64 = NativeLibrary.handle("eggdb_stmt_bind_int64", FunctionDescriptor.of(status, pointer, size, ValueLayout.JAVA_LONG));
            BIND_DOUBLE = NativeLibrary.handle("eggdb_stmt_bind_double", FunctionDescriptor.of(status, pointer, size, ValueLayout.JAVA_DOUBLE));
            BIND_BOOL = NativeLibrary.handle("eggdb_stmt_bind_bool", FunctionDescriptor.of(status, pointer, size, ValueLayout.JAVA_INT));
            BIND_TEXT = NativeLibrary.handle("eggdb_stmt_bind_text", FunctionDescriptor.of(status, pointer, size, pointer, size));
            BIND_BLOB = NativeLibrary.handle("eggdb_stmt_bind_blob", FunctionDescriptor.of(status, pointer, size, pointer, size));
            CLEAR_BINDINGS = NativeLibrary.handle("eggdb_stmt_clear_bindings", FunctionDescriptor.of(status, pointer));
            STATEMENT_EXECUTE = NativeLibrary.handle("eggdb_stmt_execute", FunctionDescriptor.of(status, pointer, pointer, pointer));
            RESET = NativeLibrary.handle("eggdb_stmt_reset", FunctionDescriptor.of(status, pointer));
            COLUMN_COUNT = NativeLibrary.handle("eggdb_stmt_column_count", FunctionDescriptor.of(status, pointer, pointer));
            COLUMN_NAME = NativeLibrary.handle("eggdb_stmt_column_name", FunctionDescriptor.of(status, pointer, size, pointer));
            STEP = NativeLibrary.handle("eggdb_stmt_step", FunctionDescriptor.of(status, pointer, pointer));
            ROW = NativeLibrary.handle("eggdb_stmt_row", FunctionDescriptor.of(status, pointer, pointer, pointer));
            FINALIZE = NativeLibrary.handle("eggdb_stmt_finalize", FunctionDescriptor.of(status, pointer));
            COPY_IN_BEGIN = NativeLibrary.handle("eggdb_sql_copy_in_begin", FunctionDescriptor.of(status, pointer, pointer, pointer));
            COPY_OUT_BEGIN = NativeLibrary.handle("eggdb_sql_copy_out_begin", FunctionDescriptor.of(status, pointer, pointer, pointer));
            COPY_PUT = NativeLibrary.handle("eggdb_copy_put", FunctionDescriptor.of(status, pointer, pointer, size));
            COPY_READ = NativeLibrary.handle("eggdb_copy_read", FunctionDescriptor.of(status, pointer, pointer, pointer));
            COPY_END = NativeLibrary.handle("eggdb_copy_end", FunctionDescriptor.of(status, pointer, pointer, pointer));
            COPY_FREE = NativeLibrary.handle("eggdb_copy_free", FunctionDescriptor.ofVoid(pointer));
        }
    }

    private interface Call<T> {
        T call(Arena arena) throws Throwable;
    }

    private static <T> T called(Call<T> call) throws SQLException {
        try (Arena arena = Arena.ofConfined()) {
            return call.call(arena);
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw new SQLException("EggDB's C library could not be called: " + error, "58000", error);
        }
    }

    private static MemorySegment address(long handle) {
        return MemorySegment.ofAddress(handle);
    }

    private static MemorySegment bytes(Arena arena, byte[] data) {
        MemorySegment copy = arena.allocate(Math.max(data.length, 1));
        MemorySegment.copy(data, 0, copy, ValueLayout.JAVA_BYTE, 0, data.length);
        return copy;
    }

    private static long handleOut(Arena arena, CheckedOut call) throws Throwable {
        MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
        check(call.call(out));
        return out.get(ValueLayout.ADDRESS, 0).address();
    }

    private interface CheckedOut {
        int call(MemorySegment out) throws Throwable;
    }

    @Override
    public long open(String path, boolean readOnly) throws SQLException {
        return called(arena -> {
            MemorySegment options = arena.allocate(16, 8);
            options.set(ValueLayout.JAVA_INT, 0, readOnly ? OPEN_READ_ONLY : 0);
            MemorySegment text = arena.allocateFrom(path);
            return handleOut(arena, out -> (int) Api.OPEN.invokeExact(text, options, out));
        });
    }

    @Override
    public long sqlOpen(long database, String dialect) throws SQLException {
        return called(arena -> {
            MemorySegment text = arena.allocateFrom(dialect);
            return handleOut(arena, out -> (int) Api.SQL_OPEN.invokeExact(address(database), text, out));
        });
    }

    @Override
    public long connect(String url) throws SQLException {
        return called(arena -> {
            MemorySegment text = arena.allocateFrom(url);
            return handleOut(arena, out -> (int) Api.SQL_CONNECT.invokeExact(text, out));
        });
    }

    @Override
    public void sqlClose(long session) {
        try {
            int ignored = (int) Api.SQL_CLOSE.invokeExact(address(session));
        } catch (Throwable ignored) {
            return;
        }
    }

    @Override
    public void close(long database) {
        try {
            int ignored = (int) Api.CLOSE.invokeExact(address(database));
        } catch (Throwable ignored) {
            return;
        }
    }

    @Override
    public List<NativeSession.StatementResult> execute(long session, String sql, List<String> parameters) throws SQLException {
        return called(arena -> {
            MemorySegment array = arena.allocate(PARAMETER_BYTES * Math.max(parameters.size(), 1), 8);
            for (int index = 0; index < parameters.size(); index++) {
                String text = parameters.get(index);
                if (text != null) {
                    byte[] encoded = text.getBytes(StandardCharsets.UTF_8);
                    array.set(ValueLayout.ADDRESS, index * PARAMETER_BYTES, bytes(arena, encoded));
                    array.set(ValueLayout.JAVA_LONG, index * PARAMETER_BYTES + 8, encoded.length);
                }
            }
            MemorySegment results = arena.allocate(ValueLayout.ADDRESS);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            check((int) Api.EXECUTE.invokeExact(address(session), arena.allocateFrom(sql), array,
                    (long) parameters.size(), results, count));
            return gathered(results.get(ValueLayout.ADDRESS, 0), count.get(ValueLayout.JAVA_LONG, 0));
        });
    }

    @Override
    public long prepare(long session, String sql) throws SQLException {
        return called(arena -> {
            byte[] text = sql.getBytes(StandardCharsets.UTF_8);
            MemorySegment copy = bytes(arena, text);
            return handleOut(arena, out -> (int) Api.PREPARE.invokeExact(address(session), copy, (long) text.length, out));
        });
    }

    @Override
    public int parameterCount(long statement) throws SQLException {
        return called(arena -> {
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            check((int) Api.PARAMETER_COUNT.invokeExact(address(statement), count));
            return (int) count.get(ValueLayout.JAVA_LONG, 0);
        });
    }

    @Override
    public void bind(long statement, Bindings bindings) throws SQLException {
        called(arena -> {
            MemorySegment handle = address(statement);
            check((int) Api.RESET.invokeExact(handle));
            check((int) Api.CLEAR_BINDINGS.invokeExact(handle));
            for (int at = 0; at < bindings.size(); at++) {
                long index = at + 1L;
                int status = switch (bindings.kinds()[at]) {
                    case Bindings.NULL -> (int) Api.BIND_NULL.invokeExact(handle, index);
                    case Bindings.BOOLEAN -> (int) Api.BIND_BOOL.invokeExact(handle, index, (int) bindings.integers()[at]);
                    case Bindings.INTEGER -> (int) Api.BIND_INT64.invokeExact(handle, index, bindings.integers()[at]);
                    case Bindings.REAL -> (int) Api.BIND_DOUBLE.invokeExact(handle, index, bindings.reals()[at]);
                    case Bindings.BLOB -> (int) Api.BIND_BLOB.invokeExact(handle, index, bytes(arena, bindings.bytes()[at]),
                            (long) bindings.bytes()[at].length);
                    default -> (int) Api.BIND_TEXT.invokeExact(handle, index, bytes(arena, bindings.bytes()[at]),
                            (long) bindings.bytes()[at].length);
                };
                check(status);
            }
            return null;
        });
    }

    @Override
    public List<NativeSession.StatementResult> run(long statement) throws SQLException {
        return called(arena -> {
            MemorySegment results = arena.allocate(ValueLayout.ADDRESS);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            check((int) Api.STATEMENT_EXECUTE.invokeExact(address(statement), results, count));
            return gathered(results.get(ValueLayout.ADDRESS, 0), count.get(ValueLayout.JAVA_LONG, 0));
        });
    }

    @Override
    public List<String> columns(long statement) throws SQLException {
        return called(arena -> {
            MemorySegment handle = address(statement);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            check((int) Api.COLUMN_COUNT.invokeExact(handle, count));
            MemorySegment name = arena.allocate(ValueLayout.ADDRESS);
            List<String> names = new ArrayList<>();
            for (long column = 0; column < count.get(ValueLayout.JAVA_LONG, 0); column++) {
                check((int) Api.COLUMN_NAME.invokeExact(handle, column, name));
                names.add(text(name.get(ValueLayout.ADDRESS, 0)));
            }
            return names;
        });
    }

    @Override
    public Object[] step(long statement) throws SQLException {
        return called(arena -> {
            MemorySegment handle = address(statement);
            MemorySegment hasRow = arena.allocate(ValueLayout.JAVA_INT);
            check((int) Api.STEP.invokeExact(handle, hasRow));
            if (hasRow.get(ValueLayout.JAVA_INT, 0) == 0) {
                return null;
            }
            MemorySegment values = arena.allocate(ValueLayout.ADDRESS);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            check((int) Api.ROW.invokeExact(handle, values, count));
            int columns = (int) count.get(ValueLayout.JAVA_LONG, 0);
            MemorySegment row = values.get(ValueLayout.ADDRESS, 0).reinterpret(VALUE_BYTES * columns);
            Object[] cells = new Object[columns];
            for (int column = 0; column < columns; column++) {
                cells[column] = converted(row, VALUE_BYTES * column);
            }
            return cells;
        });
    }

    @Override
    public void reset(long statement) {
        try {
            int ignored = (int) Api.RESET.invokeExact(address(statement));
        } catch (Throwable ignored) {
            return;
        }
    }

    @Override
    public void finalizeStatement(long statement) {
        try {
            int ignored = (int) Api.FINALIZE.invokeExact(address(statement));
        } catch (Throwable ignored) {
            return;
        }
    }

    @Override
    public long copyIn(long session, String sql, InputStream source) throws SQLException {
        return called(arena -> {
            MemorySegment text = arena.allocateFrom(sql);
            MemorySegment copy = address(handleOut(arena, out -> (int) Api.COPY_IN_BEGIN.invokeExact(address(session), text, out)));
            try {
                byte[] buffer = new byte[COPY_PIECE_BYTES];
                MemorySegment piece = arena.allocate(buffer.length);
                int read;
                while ((read = source.read(buffer)) > 0) {
                    MemorySegment.copy(buffer, 0, piece, ValueLayout.JAVA_BYTE, 0, read);
                    check((int) Api.COPY_PUT.invokeExact(copy, piece, (long) read));
                }
                return NativeSession.copiedRows(endCopy(arena, copy));
            } catch (IOException error) {
                throw new SQLException("reading the COPY data failed: " + error.getMessage(), "58030", error);
            } finally {
                Api.COPY_FREE.invokeExact(copy);
            }
        });
    }

    @Override
    public void copyOut(long session, String sql, OutputStream sink) throws SQLException {
        called(arena -> {
            MemorySegment text = arena.allocateFrom(sql);
            MemorySegment copy = address(handleOut(arena, out -> (int) Api.COPY_OUT_BEGIN.invokeExact(address(session), text, out)));
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
                return null;
            } catch (IOException error) {
                throw new SQLException("writing the COPY data failed: " + error.getMessage(), "58030", error);
            } finally {
                Api.COPY_FREE.invokeExact(copy);
            }
        });
    }

    private static List<NativeSession.StatementResult> endCopy(Arena arena, MemorySegment copy) throws Throwable {
        MemorySegment results = arena.allocate(ValueLayout.ADDRESS);
        MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
        check((int) Api.COPY_END.invokeExact(copy, results, count));
        return gathered(results.get(ValueLayout.ADDRESS, 0), count.get(ValueLayout.JAVA_LONG, 0));
    }

    private static List<NativeSession.StatementResult> gathered(MemorySegment first, long count) {
        List<NativeSession.StatementResult> gathered = new ArrayList<>();
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
            gathered.add(new NativeSession.StatementResult(tag, columns, rows));
        }
        return gathered;
    }

    private static Object converted(MemorySegment values, long at) {
        int kind = values.get(ValueLayout.JAVA_INT, at);
        if (kind == Bindings.NULL) {
            return null;
        }
        if (kind == Bindings.INTEGER) {
            return values.get(ValueLayout.JAVA_LONG, at + 8);
        }
        if (kind == Bindings.REAL) {
            return values.get(ValueLayout.JAVA_DOUBLE, at + 16);
        }
        long length = values.get(ValueLayout.JAVA_LONG, at + 32);
        byte[] data = values.get(ValueLayout.ADDRESS, at + 24).reinterpret(length).toArray(ValueLayout.JAVA_BYTE);
        return kind == Bindings.BLOB ? data : new String(data, StandardCharsets.UTF_8);
    }

    private static String text(MemorySegment pointer) {
        return pointer.equals(MemorySegment.NULL) ? "" : pointer.reinterpret(Long.MAX_VALUE).getString(0);
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

    private static void check(int status) throws SQLException {
        if (status != 0) {
            throw lastError(status);
        }
    }
}

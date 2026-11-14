package app.eggspot.eggdb;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.math.BigDecimal;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.sql.SQLException;
import java.util.ArrayList;
import java.util.List;

final class NativeStatement implements AutoCloseable {
    private static final class Api {
        static final MethodHandle PREPARE;
        static final MethodHandle PARAMETER_COUNT;
        static final MethodHandle BIND_NULL;
        static final MethodHandle BIND_INT64;
        static final MethodHandle BIND_DOUBLE;
        static final MethodHandle BIND_BOOL;
        static final MethodHandle BIND_TEXT;
        static final MethodHandle BIND_BLOB;
        static final MethodHandle CLEAR_BINDINGS;
        static final MethodHandle EXECUTE;
        static final MethodHandle RESET;
        static final MethodHandle COLUMN_COUNT;
        static final MethodHandle COLUMN_NAME;
        static final MethodHandle STEP;
        static final MethodHandle ROW;
        static final MethodHandle FINALIZE;

        static {
            ValueLayout pointer = ValueLayout.ADDRESS;
            ValueLayout status = ValueLayout.JAVA_INT;
            ValueLayout size = ValueLayout.JAVA_LONG;
            PREPARE = NativeLibrary.handle("eggdb_sql_prepare", FunctionDescriptor.of(status, pointer, pointer, size, pointer));
            PARAMETER_COUNT = NativeLibrary.handle("eggdb_stmt_parameter_count", FunctionDescriptor.of(status, pointer, pointer));
            BIND_NULL = NativeLibrary.handle("eggdb_stmt_bind_null", FunctionDescriptor.of(status, pointer, size));
            BIND_INT64 = NativeLibrary.handle("eggdb_stmt_bind_int64", FunctionDescriptor.of(status, pointer, size, ValueLayout.JAVA_LONG));
            BIND_DOUBLE = NativeLibrary.handle("eggdb_stmt_bind_double", FunctionDescriptor.of(status, pointer, size, ValueLayout.JAVA_DOUBLE));
            BIND_BOOL = NativeLibrary.handle("eggdb_stmt_bind_bool", FunctionDescriptor.of(status, pointer, size, ValueLayout.JAVA_INT));
            BIND_TEXT = NativeLibrary.handle("eggdb_stmt_bind_text", FunctionDescriptor.of(status, pointer, size, pointer, size));
            BIND_BLOB = NativeLibrary.handle("eggdb_stmt_bind_blob", FunctionDescriptor.of(status, pointer, size, pointer, size));
            CLEAR_BINDINGS = NativeLibrary.handle("eggdb_stmt_clear_bindings", FunctionDescriptor.of(status, pointer));
            EXECUTE = NativeLibrary.handle("eggdb_stmt_execute", FunctionDescriptor.of(status, pointer, pointer, pointer));
            RESET = NativeLibrary.handle("eggdb_stmt_reset", FunctionDescriptor.of(status, pointer));
            COLUMN_COUNT = NativeLibrary.handle("eggdb_stmt_column_count", FunctionDescriptor.of(status, pointer, pointer));
            COLUMN_NAME = NativeLibrary.handle("eggdb_stmt_column_name", FunctionDescriptor.of(status, pointer, size, pointer));
            STEP = NativeLibrary.handle("eggdb_stmt_step", FunctionDescriptor.of(status, pointer, pointer));
            ROW = NativeLibrary.handle("eggdb_stmt_row", FunctionDescriptor.of(status, pointer, pointer, pointer));
            FINALIZE = NativeLibrary.handle("eggdb_stmt_finalize", FunctionDescriptor.of(status, pointer));
        }
    }

    private static final BigInteger LONG_MIN = BigInteger.valueOf(Long.MIN_VALUE);
    private static final BigInteger LONG_MAX = BigInteger.valueOf(Long.MAX_VALUE);

    private final String dialect;
    private final java.util.function.Consumer<NativeStatement> onClose;
    private MemorySegment handle;
    private final int parameterCount;

    NativeStatement(MemorySegment session, String sql, String dialect, java.util.function.Consumer<NativeStatement> onClose)
            throws SQLException {
        this.dialect = dialect;
        this.onClose = onClose;
        try (Arena arena = Arena.ofConfined()) {
            byte[] text = sql.getBytes(StandardCharsets.UTF_8);
            MemorySegment copy = arena.allocate(Math.max(text.length, 1));
            MemorySegment.copy(text, 0, copy, ValueLayout.JAVA_BYTE, 0, text.length);
            MemorySegment out = arena.allocate(ValueLayout.ADDRESS);
            check((int) Api.PREPARE.invokeExact(session, copy, (long) text.length, out));
            handle = out.get(ValueLayout.ADDRESS, 0);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            int status = (int) Api.PARAMETER_COUNT.invokeExact(handle, count);
            if (status != 0) {
                SQLException error = NativeSession.lastError(status);
                close();
                throw error;
            }
            parameterCount = (int) count.get(ValueLayout.JAVA_LONG, 0);
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw NativeSession.callFailed(error);
        }
    }

    int parameterCount() {
        return parameterCount;
    }

    private MemorySegment live() throws SQLException {
        if (handle == null) {
            throw new SQLException("the prepared statement is closed", "HY010");
        }
        return handle;
    }

    void bind(List<Object> values) throws SQLException {
        MemorySegment statement = live();
        try (Arena arena = Arena.ofConfined()) {
            check((int) Api.RESET.invokeExact(statement));
            check((int) Api.CLEAR_BINDINGS.invokeExact(statement));
            for (int index = 0; index < values.size(); index++) {
                check(bindOne(arena, statement, index + 1L, values.get(index)));
            }
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw NativeSession.callFailed(error);
        }
    }

    private int bindOne(Arena arena, MemorySegment statement, long index, Object value) throws Throwable {
        if (value == null) {
            return (int) Api.BIND_NULL.invokeExact(statement, index);
        }
        if (value instanceof Boolean flag) {
            return (int) Api.BIND_BOOL.invokeExact(statement, index, flag ? 1 : 0);
        }
        if (value instanceof Long || value instanceof Integer || value instanceof Short || value instanceof Byte) {
            return (int) Api.BIND_INT64.invokeExact(statement, index, ((Number) value).longValue());
        }
        if (value instanceof BigInteger big && big.compareTo(LONG_MIN) >= 0 && big.compareTo(LONG_MAX) <= 0) {
            return (int) Api.BIND_INT64.invokeExact(statement, index, big.longValue());
        }
        if (value instanceof Double || value instanceof Float) {
            return (int) Api.BIND_DOUBLE.invokeExact(statement, index, ((Number) value).doubleValue());
        }
        if (value instanceof byte[] bytes) {
            MemorySegment copy = arena.allocate(Math.max(bytes.length, 1));
            MemorySegment.copy(bytes, 0, copy, ValueLayout.JAVA_BYTE, 0, bytes.length);
            return (int) Api.BIND_BLOB.invokeExact(statement, index, copy, (long) bytes.length);
        }
        String text = value instanceof BigDecimal decimal ? decimal.toPlainString() : Parameters.text(value, dialect);
        byte[] bytes = text.getBytes(StandardCharsets.UTF_8);
        MemorySegment copy = arena.allocate(Math.max(bytes.length, 1));
        MemorySegment.copy(bytes, 0, copy, ValueLayout.JAVA_BYTE, 0, bytes.length);
        return (int) Api.BIND_TEXT.invokeExact(statement, index, copy, (long) bytes.length);
    }

    List<NativeSession.StatementResult> execute() throws SQLException {
        MemorySegment statement = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment results = arena.allocate(ValueLayout.ADDRESS);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            check((int) Api.EXECUTE.invokeExact(statement, results, count));
            return NativeSession.gathered(results.get(ValueLayout.ADDRESS, 0), count.get(ValueLayout.JAVA_LONG, 0));
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw NativeSession.callFailed(error);
        }
    }

    List<String> columns() throws SQLException {
        MemorySegment statement = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            check((int) Api.COLUMN_COUNT.invokeExact(statement, count));
            MemorySegment name = arena.allocate(ValueLayout.ADDRESS);
            List<String> names = new ArrayList<>();
            for (long column = 0; column < count.get(ValueLayout.JAVA_LONG, 0); column++) {
                check((int) Api.COLUMN_NAME.invokeExact(statement, column, name));
                names.add(NativeSession.text(name.get(ValueLayout.ADDRESS, 0)));
            }
            return names;
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw NativeSession.callFailed(error);
        }
    }

    Object[] step() throws SQLException {
        MemorySegment statement = live();
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment hasRow = arena.allocate(ValueLayout.JAVA_INT);
            check((int) Api.STEP.invokeExact(statement, hasRow));
            if (hasRow.get(ValueLayout.JAVA_INT, 0) == 0) {
                return null;
            }
            MemorySegment values = arena.allocate(ValueLayout.ADDRESS);
            MemorySegment count = arena.allocate(ValueLayout.JAVA_LONG);
            check((int) Api.ROW.invokeExact(statement, values, count));
            int columns = (int) count.get(ValueLayout.JAVA_LONG, 0);
            MemorySegment row = values.get(ValueLayout.ADDRESS, 0).reinterpret(NativeSession.VALUE_BYTES * columns);
            Object[] cells = new Object[columns];
            for (int column = 0; column < columns; column++) {
                cells[column] = NativeSession.converted(row, NativeSession.VALUE_BYTES * column);
            }
            return cells;
        } catch (SQLException error) {
            throw error;
        } catch (Throwable error) {
            throw NativeSession.callFailed(error);
        }
    }

    void reset() {
        if (handle == null) {
            return;
        }
        try {
            int ignored = (int) Api.RESET.invokeExact(handle);
        } catch (Throwable ignored) {
            return;
        }
    }

    @Override
    public void close() {
        if (handle == null) {
            return;
        }
        try {
            int ignored = (int) Api.FINALIZE.invokeExact(handle);
        } catch (Throwable ignored) {
            return;
        } finally {
            handle = null;
            onClose.accept(this);
        }
    }

    private static void check(int status) throws SQLException {
        if (status != 0) {
            throw NativeSession.lastError(status);
        }
    }
}

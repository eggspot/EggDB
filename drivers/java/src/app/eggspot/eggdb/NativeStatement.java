package app.eggspot.eggdb;

import java.sql.SQLException;
import java.util.List;
import java.util.function.Consumer;

final class NativeStatement implements AutoCloseable {
    private static final long FINALIZED = 0;

    private final NativeCalls calls;
    private final String dialect;
    private final Consumer<NativeStatement> onClose;
    private final int parameterCount;
    private long handle;

    NativeStatement(NativeCalls calls, long session, String sql, String dialect, Consumer<NativeStatement> onClose)
            throws SQLException {
        this.calls = calls;
        this.dialect = dialect;
        this.onClose = onClose;
        this.handle = calls.prepare(session, sql);
        int count;
        try {
            count = calls.parameterCount(handle);
        } catch (SQLException error) {
            calls.finalizeStatement(handle);
            handle = FINALIZED;
            throw error;
        }
        this.parameterCount = count;
    }

    int parameterCount() {
        return parameterCount;
    }

    private long live() throws SQLException {
        if (handle == FINALIZED) {
            throw new SQLException("the prepared statement is closed", "HY010");
        }
        return handle;
    }

    void bind(List<Object> values) throws SQLException {
        calls.bind(live(), Bindings.of(values, dialect));
    }

    List<NativeSession.StatementResult> execute() throws SQLException {
        return calls.run(live());
    }

    List<String> columns() throws SQLException {
        return calls.columns(live());
    }

    Object[] step() throws SQLException {
        return calls.step(live());
    }

    void reset() {
        if (handle != FINALIZED) {
            calls.reset(handle);
        }
    }

    @Override
    public void close() {
        if (handle == FINALIZED) {
            return;
        }
        calls.finalizeStatement(handle);
        handle = FINALIZED;
        onClose.accept(this);
    }
}

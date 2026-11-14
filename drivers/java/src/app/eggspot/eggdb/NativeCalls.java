package app.eggspot.eggdb;

import java.io.InputStream;
import java.io.OutputStream;
import java.sql.SQLException;
import java.util.List;

interface NativeCalls {
    int FOREIGN_FUNCTION_RELEASE = 22;

    static NativeCalls get() {
        return Chosen.CALLS;
    }

    final class Chosen {
        static final NativeCalls CALLS = choose();

        private Chosen() {
        }

        private static NativeCalls choose() {
            String forced = System.getProperty("eggdb.binding", "");
            boolean foreign = forced.isEmpty() ? Runtime.version().feature() >= FOREIGN_FUNCTION_RELEASE : forced.equals("ffm");
            if (!foreign) {
                return new JniCalls();
            }
            try {
                return (NativeCalls) Class.forName("app.eggspot.eggdb.FfmCalls").getDeclaredConstructor().newInstance();
            } catch (ReflectiveOperationException | LinkageError unavailable) {
                return new JniCalls();
            }
        }
    }

    long open(String path, boolean readOnly) throws SQLException;

    long sqlOpen(long database, String dialect) throws SQLException;

    long connect(String url) throws SQLException;

    void sqlClose(long session);

    void close(long database);

    List<NativeSession.StatementResult> execute(long session, String sql, List<String> parameters) throws SQLException;

    long prepare(long session, String sql) throws SQLException;

    int parameterCount(long statement) throws SQLException;

    void bind(long statement, Bindings bindings) throws SQLException;

    List<NativeSession.StatementResult> run(long statement) throws SQLException;

    List<String> columns(long statement) throws SQLException;

    Object[] step(long statement) throws SQLException;

    void reset(long statement);

    void finalizeStatement(long statement);

    long copyIn(long session, String sql, InputStream source) throws SQLException;

    void copyOut(long session, String sql, OutputStream sink) throws SQLException;
}

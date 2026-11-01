package app.eggspot.eggdb;

import java.sql.Array;
import java.sql.Blob;
import java.sql.CallableStatement;
import java.sql.Clob;
import java.sql.Connection;
import java.sql.DatabaseMetaData;
import java.sql.NClob;
import java.sql.PreparedStatement;
import java.sql.ResultSet;
import java.sql.SQLClientInfoException;
import java.sql.SQLException;
import java.sql.SQLFeatureNotSupportedException;
import java.sql.SQLWarning;
import java.sql.SQLXML;
import java.sql.Savepoint;
import java.sql.Statement;
import java.sql.Struct;
import java.util.List;
import java.util.Map;
import java.util.Properties;
import java.util.concurrent.Executor;

/** A connection to an embedded EggDB database, made by {@link EggDbDriver}. */
public final class EggDbConnection implements Connection {
    private final NativeSession session;
    private final String url;
    private final boolean readOnly;
    private boolean autoCommit = true;
    private boolean inTransaction;
    private int savepoints;

    EggDbConnection(NativeSession session, String url, boolean readOnly) {
        this.session = session;
        this.url = url;
        this.readOnly = readOnly;
    }

    List<NativeSession.StatementResult> run(String sql, List<Object> parameters) throws SQLException {
        beginIfNeeded();
        return session.execute(sql, parameters);
    }

    private void beginIfNeeded() throws SQLException {
        if (!autoCommit && !inTransaction) {
            session.execute("BEGIN", List.of());
            inTransaction = true;
        }
    }

    String url() {
        return url;
    }

    private void checkOpen() throws SQLException {
        if (session.isClosed()) {
            throw new SQLException("the connection is closed", "08003");
        }
    }

    private static SQLFeatureNotSupportedException unsupported(String what) {
        return new SQLFeatureNotSupportedException(what + " is not supported by EggDB's embedded driver yet", "0A000");
    }

    @Override
    public Statement createStatement() throws SQLException {
        checkOpen();
        return new EggDbStatement(this, null);
    }

    @Override
    public PreparedStatement prepareStatement(String sql) throws SQLException {
        checkOpen();
        return new EggDbStatement(this, sql);
    }

    @Override
    public CallableStatement prepareCall(String sql) throws SQLException {
        throw unsupported("prepareCall");
    }

    @Override
    public String nativeSQL(String sql) {
        return sql;
    }

    @Override
    public void setAutoCommit(boolean autoCommit) throws SQLException {
        checkOpen();
        if (autoCommit && inTransaction) {
            commit();
        }
        this.autoCommit = autoCommit;
    }

    @Override
    public boolean getAutoCommit() {
        return autoCommit;
    }

    @Override
    public void commit() throws SQLException {
        checkOpen();
        if (inTransaction) {
            session.execute("COMMIT", List.of());
            inTransaction = false;
        }
    }

    @Override
    public void rollback() throws SQLException {
        checkOpen();
        if (inTransaction) {
            session.execute("ROLLBACK", List.of());
            inTransaction = false;
        }
    }

    @Override
    public void close() throws SQLException {
        if (session.isClosed()) {
            return;
        }
        if (inTransaction) {
            try {
                session.execute("ROLLBACK", List.of());
            } finally {
                inTransaction = false;
                session.close();
            }
            return;
        }
        session.close();
    }

    @Override
    public boolean isClosed() {
        return session.isClosed();
    }

    @Override
    public DatabaseMetaData getMetaData() throws SQLException {
        throw unsupported("getMetaData");
    }

    @Override
    public void setReadOnly(boolean readOnly) throws SQLException {
        if (readOnly != this.readOnly) {
            throw unsupported("changing read-only mode after connecting (use mode=ro in the URL)");
        }
    }

    @Override
    public boolean isReadOnly() {
        return readOnly;
    }

    @Override
    public void setCatalog(String catalog) {
    }

    @Override
    public String getCatalog() {
        return null;
    }

    @Override
    public void setTransactionIsolation(int level) throws SQLException {
        if (level != TRANSACTION_SERIALIZABLE) {
            throw unsupported("isolation levels other than serializable");
        }
    }

    @Override
    public int getTransactionIsolation() {
        return TRANSACTION_SERIALIZABLE;
    }

    @Override
    public SQLWarning getWarnings() {
        return null;
    }

    @Override
    public void clearWarnings() {
    }

    @Override
    public Statement createStatement(int resultSetType, int resultSetConcurrency) throws SQLException {
        checkCursor(resultSetType, resultSetConcurrency);
        return createStatement();
    }

    @Override
    public PreparedStatement prepareStatement(String sql, int resultSetType, int resultSetConcurrency) throws SQLException {
        checkCursor(resultSetType, resultSetConcurrency);
        return prepareStatement(sql);
    }

    @Override
    public CallableStatement prepareCall(String sql, int resultSetType, int resultSetConcurrency) throws SQLException {
        throw unsupported("prepareCall");
    }

    private static void checkCursor(int resultSetType, int resultSetConcurrency) throws SQLException {
        if (resultSetType != ResultSet.TYPE_FORWARD_ONLY || resultSetConcurrency != ResultSet.CONCUR_READ_ONLY) {
            throw unsupported("scrollable or updatable result sets");
        }
    }

    @Override
    public Map<String, Class<?>> getTypeMap() {
        return Map.of();
    }

    @Override
    public void setTypeMap(Map<String, Class<?>> map) throws SQLException {
        throw unsupported("type maps");
    }

    @Override
    public void setHoldability(int holdability) throws SQLException {
        if (holdability != ResultSet.CLOSE_CURSORS_AT_COMMIT) {
            throw unsupported("holding cursors over commits");
        }
    }

    @Override
    public int getHoldability() {
        return ResultSet.CLOSE_CURSORS_AT_COMMIT;
    }

    @Override
    public Savepoint setSavepoint() throws SQLException {
        int id = ++savepoints;
        return named(new EggDbSavepoint(id, null));
    }

    @Override
    public Savepoint setSavepoint(String name) throws SQLException {
        return named(new EggDbSavepoint(++savepoints, name));
    }

    private Savepoint named(EggDbSavepoint savepoint) throws SQLException {
        if (autoCommit) {
            throw new SQLException("a savepoint needs a transaction (auto-commit is on)", "25000");
        }
        run("SAVEPOINT " + savepoint.identifier(), List.of());
        return savepoint;
    }

    @Override
    public void rollback(Savepoint savepoint) throws SQLException {
        run("ROLLBACK TO SAVEPOINT " + ((EggDbSavepoint) savepoint).identifier(), List.of());
    }

    @Override
    public void releaseSavepoint(Savepoint savepoint) throws SQLException {
        run("RELEASE SAVEPOINT " + ((EggDbSavepoint) savepoint).identifier(), List.of());
    }

    @Override
    public Statement createStatement(int resultSetType, int resultSetConcurrency, int resultSetHoldability) throws SQLException {
        setHoldability(resultSetHoldability);
        return createStatement(resultSetType, resultSetConcurrency);
    }

    @Override
    public PreparedStatement prepareStatement(String sql, int resultSetType, int resultSetConcurrency, int resultSetHoldability)
            throws SQLException {
        setHoldability(resultSetHoldability);
        return prepareStatement(sql, resultSetType, resultSetConcurrency);
    }

    @Override
    public CallableStatement prepareCall(String sql, int resultSetType, int resultSetConcurrency, int resultSetHoldability)
            throws SQLException {
        throw unsupported("prepareCall");
    }

    @Override
    public PreparedStatement prepareStatement(String sql, int autoGeneratedKeys) throws SQLException {
        if (autoGeneratedKeys == Statement.RETURN_GENERATED_KEYS) {
            throw unsupported("generated keys (use RETURNING)");
        }
        return prepareStatement(sql);
    }

    @Override
    public PreparedStatement prepareStatement(String sql, int[] columnIndexes) throws SQLException {
        throw unsupported("generated keys (use RETURNING)");
    }

    @Override
    public PreparedStatement prepareStatement(String sql, String[] columnNames) throws SQLException {
        throw unsupported("generated keys (use RETURNING)");
    }

    @Override
    public Clob createClob() throws SQLException {
        throw unsupported("Clob");
    }

    @Override
    public Blob createBlob() throws SQLException {
        throw unsupported("Blob");
    }

    @Override
    public NClob createNClob() throws SQLException {
        throw unsupported("NClob");
    }

    @Override
    public SQLXML createSQLXML() throws SQLException {
        throw unsupported("SQLXML");
    }

    @Override
    public boolean isValid(int timeout) {
        return !session.isClosed();
    }

    @Override
    public void setClientInfo(String name, String value) throws SQLClientInfoException {
        throw new SQLClientInfoException();
    }

    @Override
    public void setClientInfo(Properties properties) throws SQLClientInfoException {
        throw new SQLClientInfoException();
    }

    @Override
    public String getClientInfo(String name) {
        return null;
    }

    @Override
    public Properties getClientInfo() {
        return new Properties();
    }

    @Override
    public Array createArrayOf(String typeName, Object[] elements) throws SQLException {
        throw unsupported("arrays");
    }

    @Override
    public Struct createStruct(String typeName, Object[] attributes) throws SQLException {
        throw unsupported("structs");
    }

    @Override
    public void setSchema(String schema) {
    }

    @Override
    public String getSchema() {
        return null;
    }

    @Override
    public void abort(Executor executor) {
        session.close();
    }

    @Override
    public void setNetworkTimeout(Executor executor, int milliseconds) {
    }

    @Override
    public int getNetworkTimeout() {
        return 0;
    }

    @Override
    public <T> T unwrap(Class<T> type) throws SQLException {
        if (type.isInstance(this)) {
            return type.cast(this);
        }
        throw new SQLException("not a wrapper for " + type.getName());
    }

    @Override
    public boolean isWrapperFor(Class<?> type) {
        return type.isInstance(this);
    }
}

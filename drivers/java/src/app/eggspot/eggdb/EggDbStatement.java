package app.eggspot.eggdb;

import java.io.InputStream;
import java.io.Reader;
import java.math.BigDecimal;
import java.net.URL;
import java.sql.Array;
import java.sql.Blob;
import java.sql.Clob;
import java.sql.Connection;
import java.sql.Date;
import java.sql.NClob;
import java.sql.ParameterMetaData;
import java.sql.PreparedStatement;
import java.sql.Ref;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.RowId;
import java.sql.SQLException;
import java.sql.SQLFeatureNotSupportedException;
import java.sql.SQLWarning;
import java.sql.SQLXML;
import java.sql.Time;
import java.sql.Timestamp;
import java.util.ArrayList;
import java.util.Calendar;
import java.util.List;

/** A statement on an {@link EggDbConnection}; prepared statements bind {@code ?} placeholders in order. */
public final class EggDbStatement implements PreparedStatement {
    private final EggDbConnection connection;
    private final String preparedSql;
    private final List<Object> parameters = new ArrayList<>();
    private final List<List<Object>> batch = new ArrayList<>();
    private final List<String> textBatch = new ArrayList<>();
    private List<NativeSession.StatementResult> results = List.of();
    private int current;
    private EggDbResultSet resultSet;
    private int maxRows;
    private boolean closed;
    private boolean closeOnCompletion;

    EggDbStatement(EggDbConnection connection, String preparedSql) {
        this.connection = connection;
        this.preparedSql = preparedSql;
    }

    private static SQLFeatureNotSupportedException unsupported(String what) {
        return new SQLFeatureNotSupportedException(what + " is not supported by EggDB's embedded driver yet", "0A000");
    }

    private void checkOpen() throws SQLException {
        if (closed) {
            throw new SQLException("the statement is closed", "HY010");
        }
    }

    private String prepared() throws SQLException {
        if (preparedSql == null) {
            throw new SQLException("this statement was not prepared; pass the SQL to execute", "HY010");
        }
        return preparedSql;
    }

    private boolean run(String sql, List<Object> values) throws SQLException {
        checkOpen();
        closeResultSet();
        results = connection.run(sql, values);
        current = 0;
        return currentReturnsRows();
    }

    private boolean currentReturnsRows() {
        return current < results.size() && results.get(current).returnsRows();
    }

    private void closeResultSet() {
        if (resultSet != null) {
            resultSet.closeQuietly();
            resultSet = null;
        }
    }

    void resultSetClosed(EggDbResultSet closedSet) throws SQLException {
        if (resultSet == closedSet) {
            resultSet = null;
            if (closeOnCompletion) {
                close();
            }
        }
    }

    private ResultSet query(String sql, List<Object> values) throws SQLException {
        run(sql, values);
        for (int index = results.size() - 1; index >= 0; index--) {
            if (results.get(index).returnsRows()) {
                current = index;
                return getResultSet();
            }
        }
        throw new SQLException("the statement returned no rows", "02000");
    }

    private int update(String sql, List<Object> values) throws SQLException {
        run(sql, values);
        int total = 0;
        for (NativeSession.StatementResult result : results) {
            total += result.changedRows();
        }
        return total;
    }

    private void bind(int index, Object value) throws SQLException {
        checkOpen();
        if (index < 1) {
            throw new SQLException("parameter indexes start at 1", "07009");
        }
        while (parameters.size() < index) {
            parameters.add(null);
        }
        parameters.set(index - 1, value);
    }

    @Override
    public ResultSet executeQuery(String sql) throws SQLException {
        return query(sql, List.of());
    }

    @Override
    public int executeUpdate(String sql) throws SQLException {
        return update(sql, List.of());
    }

    @Override
    public boolean execute(String sql) throws SQLException {
        return run(sql, List.of());
    }

    @Override
    public ResultSet executeQuery() throws SQLException {
        return query(prepared(), parameters);
    }

    @Override
    public int executeUpdate() throws SQLException {
        return update(prepared(), parameters);
    }

    @Override
    public boolean execute() throws SQLException {
        return run(prepared(), parameters);
    }

    @Override
    public void close() {
        closeResultSet();
        closed = true;
    }

    @Override
    public boolean isClosed() {
        return closed;
    }

    @Override
    public ResultSet getResultSet() throws SQLException {
        checkOpen();
        if (!currentReturnsRows()) {
            return null;
        }
        if (resultSet == null) {
            NativeSession.StatementResult result = results.get(current);
            List<Object[]> rows = maxRows > 0 && result.rows().size() > maxRows ? result.rows().subList(0, maxRows) : result.rows();
            resultSet = new EggDbResultSet(this, result.columns(), rows);
        }
        return resultSet;
    }

    @Override
    public int getUpdateCount() throws SQLException {
        checkOpen();
        if (current >= results.size() || results.get(current).returnsRows()) {
            return -1;
        }
        return results.get(current).changedRows();
    }

    @Override
    public boolean getMoreResults() throws SQLException {
        return getMoreResults(CLOSE_CURRENT_RESULT);
    }

    @Override
    public boolean getMoreResults(int current) throws SQLException {
        checkOpen();
        closeResultSet();
        if (this.current < results.size()) {
            this.current++;
        }
        return currentReturnsRows();
    }

    @Override
    public void addBatch(String sql) throws SQLException {
        checkOpen();
        textBatch.add(sql);
    }

    @Override
    public void addBatch() throws SQLException {
        checkOpen();
        batch.add(new ArrayList<>(parameters));
    }

    @Override
    public void clearBatch() {
        batch.clear();
        textBatch.clear();
    }

    @Override
    public int[] executeBatch() throws SQLException {
        checkOpen();
        int[] counts = new int[batch.size() + textBatch.size()];
        int at = 0;
        try {
            for (String sql : textBatch) {
                counts[at++] = update(sql, List.of());
            }
            for (List<Object> values : batch) {
                counts[at++] = update(prepared(), values);
            }
        } finally {
            clearBatch();
        }
        return counts;
    }

    @Override
    public Connection getConnection() {
        return connection;
    }

    @Override
    public void clearParameters() {
        parameters.clear();
    }

    @Override
    public void setNull(int parameterIndex, int sqlType) throws SQLException {
        bind(parameterIndex, null);
    }

    @Override
    public void setNull(int parameterIndex, int sqlType, String typeName) throws SQLException {
        bind(parameterIndex, null);
    }

    @Override
    public void setBoolean(int parameterIndex, boolean value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setByte(int parameterIndex, byte value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setShort(int parameterIndex, short value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setInt(int parameterIndex, int value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setLong(int parameterIndex, long value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setFloat(int parameterIndex, float value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setDouble(int parameterIndex, double value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setBigDecimal(int parameterIndex, BigDecimal value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setString(int parameterIndex, String value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setNString(int parameterIndex, String value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setBytes(int parameterIndex, byte[] value) throws SQLException {
        bind(parameterIndex, value == null ? null : value.clone());
    }

    @Override
    public void setDate(int parameterIndex, Date value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setTime(int parameterIndex, Time value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setTimestamp(int parameterIndex, Timestamp value) throws SQLException {
        bind(parameterIndex, value);
    }

    @Override
    public void setObject(int parameterIndex, Object value) throws SQLException {
        if (value != null && !(value instanceof Number || value instanceof Boolean || value instanceof CharSequence
                || value instanceof byte[] || value instanceof java.util.Date || value instanceof java.time.temporal.Temporal
                || value instanceof java.util.UUID)) {
            throw unsupported("binding a " + value.getClass().getName());
        }
        bind(parameterIndex, value instanceof byte[] bytes ? bytes.clone() : value);
    }

    @Override
    public void setObject(int parameterIndex, Object value, int targetSqlType) throws SQLException {
        setObject(parameterIndex, value);
    }

    @Override
    public void setObject(int parameterIndex, Object value, int targetSqlType, int scaleOrLength) throws SQLException {
        setObject(parameterIndex, value);
    }

    @Override
    public void setAsciiStream(int parameterIndex, InputStream x, int length) throws SQLException {
        throw unsupported("setAsciiStream");
    }

    @Override
    public void setUnicodeStream(int parameterIndex, InputStream x, int length) throws SQLException {
        throw unsupported("setUnicodeStream");
    }

    @Override
    public void setBinaryStream(int parameterIndex, InputStream x, int length) throws SQLException {
        throw unsupported("setBinaryStream");
    }

    @Override
    public void setCharacterStream(int parameterIndex, Reader reader, int length) throws SQLException {
        throw unsupported("setCharacterStream");
    }

    @Override
    public void setRef(int parameterIndex, Ref x) throws SQLException {
        throw unsupported("setRef");
    }

    @Override
    public void setBlob(int parameterIndex, Blob x) throws SQLException {
        throw unsupported("setBlob");
    }

    @Override
    public void setClob(int parameterIndex, Clob x) throws SQLException {
        throw unsupported("setClob");
    }

    @Override
    public void setArray(int parameterIndex, Array x) throws SQLException {
        throw unsupported("setArray");
    }

    @Override
    public void setDate(int parameterIndex, Date x, Calendar calendar) throws SQLException {
        throw unsupported("setDate");
    }

    @Override
    public void setTime(int parameterIndex, Time x, Calendar calendar) throws SQLException {
        throw unsupported("setTime");
    }

    @Override
    public void setTimestamp(int parameterIndex, Timestamp x, Calendar calendar) throws SQLException {
        throw unsupported("setTimestamp");
    }

    @Override
    public void setURL(int parameterIndex, URL x) throws SQLException {
        throw unsupported("setURL");
    }

    @Override
    public void setRowId(int parameterIndex, RowId x) throws SQLException {
        throw unsupported("setRowId");
    }

    @Override
    public void setNCharacterStream(int parameterIndex, Reader value, long length) throws SQLException {
        throw unsupported("setNCharacterStream");
    }

    @Override
    public void setNClob(int parameterIndex, NClob value) throws SQLException {
        throw unsupported("setNClob");
    }

    @Override
    public void setClob(int parameterIndex, Reader reader, long length) throws SQLException {
        throw unsupported("setClob");
    }

    @Override
    public void setBlob(int parameterIndex, InputStream inputStream, long length) throws SQLException {
        throw unsupported("setBlob");
    }

    @Override
    public void setNClob(int parameterIndex, Reader reader, long length) throws SQLException {
        throw unsupported("setNClob");
    }

    @Override
    public void setSQLXML(int parameterIndex, SQLXML xmlObject) throws SQLException {
        throw unsupported("setSQLXML");
    }

    @Override
    public void setAsciiStream(int parameterIndex, InputStream x, long length) throws SQLException {
        throw unsupported("setAsciiStream");
    }

    @Override
    public void setBinaryStream(int parameterIndex, InputStream x, long length) throws SQLException {
        throw unsupported("setBinaryStream");
    }

    @Override
    public void setCharacterStream(int parameterIndex, Reader reader, long length) throws SQLException {
        throw unsupported("setCharacterStream");
    }

    @Override
    public void setAsciiStream(int parameterIndex, InputStream x) throws SQLException {
        throw unsupported("setAsciiStream");
    }

    @Override
    public void setBinaryStream(int parameterIndex, InputStream x) throws SQLException {
        throw unsupported("setBinaryStream");
    }

    @Override
    public void setCharacterStream(int parameterIndex, Reader reader) throws SQLException {
        throw unsupported("setCharacterStream");
    }

    @Override
    public void setNCharacterStream(int parameterIndex, Reader value) throws SQLException {
        throw unsupported("setNCharacterStream");
    }

    @Override
    public void setClob(int parameterIndex, Reader reader) throws SQLException {
        throw unsupported("setClob");
    }

    @Override
    public void setBlob(int parameterIndex, InputStream inputStream) throws SQLException {
        throw unsupported("setBlob");
    }

    @Override
    public void setNClob(int parameterIndex, Reader reader) throws SQLException {
        throw unsupported("setNClob");
    }

    @Override
    public ResultSetMetaData getMetaData() throws SQLException {
        throw unsupported("metadata before running a statement");
    }

    @Override
    public ParameterMetaData getParameterMetaData() throws SQLException {
        throw unsupported("parameter metadata");
    }

    @Override
    public int getMaxFieldSize() {
        return 0;
    }

    @Override
    public void setMaxFieldSize(int max) {
    }

    @Override
    public int getMaxRows() {
        return maxRows;
    }

    @Override
    public void setMaxRows(int max) throws SQLException {
        if (max < 0) {
            throw new SQLException("the row limit cannot be negative", "HY024");
        }
        maxRows = max;
    }

    @Override
    public void setEscapeProcessing(boolean enable) {
    }

    @Override
    public int getQueryTimeout() {
        return 0;
    }

    @Override
    public void setQueryTimeout(int seconds) {
    }

    @Override
    public void cancel() throws SQLException {
        throw unsupported("cancel");
    }

    @Override
    public SQLWarning getWarnings() {
        return null;
    }

    @Override
    public void clearWarnings() {
    }

    @Override
    public void setCursorName(String name) throws SQLException {
        throw unsupported("named cursors");
    }

    @Override
    public void setFetchDirection(int direction) throws SQLException {
        if (direction != ResultSet.FETCH_FORWARD) {
            throw unsupported("fetching backwards");
        }
    }

    @Override
    public int getFetchDirection() {
        return ResultSet.FETCH_FORWARD;
    }

    @Override
    public void setFetchSize(int rows) {
    }

    @Override
    public int getFetchSize() {
        return 0;
    }

    @Override
    public int getResultSetConcurrency() {
        return ResultSet.CONCUR_READ_ONLY;
    }

    @Override
    public int getResultSetType() {
        return ResultSet.TYPE_FORWARD_ONLY;
    }

    @Override
    public ResultSet getGeneratedKeys() throws SQLException {
        throw unsupported("generated keys (use RETURNING)");
    }

    @Override
    public int executeUpdate(String sql, int autoGeneratedKeys) throws SQLException {
        if (autoGeneratedKeys == RETURN_GENERATED_KEYS) {
            throw unsupported("generated keys (use RETURNING)");
        }
        return executeUpdate(sql);
    }

    @Override
    public int executeUpdate(String sql, int[] columnIndexes) throws SQLException {
        throw unsupported("generated keys (use RETURNING)");
    }

    @Override
    public int executeUpdate(String sql, String[] columnNames) throws SQLException {
        throw unsupported("generated keys (use RETURNING)");
    }

    @Override
    public boolean execute(String sql, int autoGeneratedKeys) throws SQLException {
        if (autoGeneratedKeys == RETURN_GENERATED_KEYS) {
            throw unsupported("generated keys (use RETURNING)");
        }
        return execute(sql);
    }

    @Override
    public boolean execute(String sql, int[] columnIndexes) throws SQLException {
        throw unsupported("generated keys (use RETURNING)");
    }

    @Override
    public boolean execute(String sql, String[] columnNames) throws SQLException {
        throw unsupported("generated keys (use RETURNING)");
    }

    @Override
    public int getResultSetHoldability() {
        return ResultSet.CLOSE_CURSORS_AT_COMMIT;
    }

    @Override
    public void setPoolable(boolean poolable) {
    }

    @Override
    public boolean isPoolable() {
        return preparedSql != null;
    }

    @Override
    public void closeOnCompletion() {
        closeOnCompletion = true;
    }

    @Override
    public boolean isCloseOnCompletion() {
        return closeOnCompletion;
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

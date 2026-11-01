package app.eggspot.eggdb;

import java.io.ByteArrayInputStream;
import java.io.InputStream;
import java.io.Reader;
import java.io.StringReader;
import java.math.BigDecimal;
import java.math.RoundingMode;
import java.net.URL;
import java.nio.charset.StandardCharsets;
import java.sql.Array;
import java.sql.Blob;
import java.sql.Clob;
import java.sql.Date;
import java.sql.NClob;
import java.sql.Ref;
import java.sql.ResultSet;
import java.sql.ResultSetMetaData;
import java.sql.RowId;
import java.sql.SQLException;
import java.sql.SQLFeatureNotSupportedException;
import java.sql.SQLWarning;
import java.sql.SQLXML;
import java.sql.Statement;
import java.sql.Time;
import java.sql.Timestamp;
import java.util.Calendar;
import java.util.List;
import java.util.Locale;
import java.util.Map;

/** The rows of a statement, read forward; values arrive as {@code Long}, {@code Double}, {@code String} or {@code byte[]}. */
public final class EggDbResultSet implements ResultSet {
    private final EggDbStatement statement;
    private final List<String> columns;
    private final List<Object[]> rows;
    private int row = -1;
    private boolean closed;
    private boolean wasNull;

    EggDbResultSet(EggDbStatement statement, List<String> columns, List<Object[]> rows) {
        this.statement = statement;
        this.columns = columns;
        this.rows = rows;
    }

    private static SQLFeatureNotSupportedException unsupported(String what) {
        return new SQLFeatureNotSupportedException(what + " is not supported by EggDB's embedded driver yet", "0A000");
    }

    void closeQuietly() {
        closed = true;
    }

    private Object value(int columnIndex) throws SQLException {
        if (closed) {
            throw new SQLException("the result set is closed", "HY010");
        }
        if (row < 0 || row >= rows.size()) {
            throw new SQLException("the result set is not on a row", "24000");
        }
        if (columnIndex < 1 || columnIndex > columns.size()) {
            throw new SQLException("no column " + columnIndex, "07009");
        }
        Object value = rows.get(row)[columnIndex - 1];
        wasNull = value == null;
        return value;
    }

    private Number number(int columnIndex) throws SQLException {
        Object value = value(columnIndex);
        if (value == null) {
            return 0L;
        }
        if (value instanceof Number number) {
            return number;
        }
        try {
            return new BigDecimal(value.toString().trim());
        } catch (NumberFormatException notNumber) {
            throw new SQLException("column " + columnIndex + " is not a number: " + value, "22018");
        }
    }

    List<String> columns() {
        return columns;
    }

    List<Object[]> rows() {
        return rows;
    }

    @Override
    public boolean next() throws SQLException {
        if (closed) {
            throw new SQLException("the result set is closed", "HY010");
        }
        if (row < rows.size()) {
            row++;
        }
        return row < rows.size();
    }

    @Override
    public void close() throws SQLException {
        if (!closed) {
            closed = true;
            statement.resultSetClosed(this);
        }
    }

    @Override
    public boolean isClosed() {
        return closed;
    }

    @Override
    public boolean wasNull() {
        return wasNull;
    }

    @Override
    public int findColumn(String columnLabel) throws SQLException {
        for (int index = 0; index < columns.size(); index++) {
            if (columns.get(index).equalsIgnoreCase(columnLabel)) {
                return index + 1;
            }
        }
        throw new SQLException("no column named " + columnLabel, "42703");
    }

    @Override
    public String getString(int columnIndex) throws SQLException {
        Object value = value(columnIndex);
        if (value == null) {
            return null;
        }
        if (value instanceof byte[] bytes) {
            return new String(bytes, StandardCharsets.UTF_8);
        }
        return value.toString();
    }

    @Override
    public boolean getBoolean(int columnIndex) throws SQLException {
        Object value = value(columnIndex);
        if (value == null) {
            return false;
        }
        if (value instanceof Number number) {
            return number.longValue() != 0;
        }
        String text = value.toString().trim().toLowerCase(Locale.ROOT);
        return text.equals("t") || text.equals("true") || text.equals("1") || text.equals("y") || text.equals("yes");
    }

    @Override
    public byte getByte(int columnIndex) throws SQLException {
        return number(columnIndex).byteValue();
    }

    @Override
    public short getShort(int columnIndex) throws SQLException {
        return number(columnIndex).shortValue();
    }

    @Override
    public int getInt(int columnIndex) throws SQLException {
        return number(columnIndex).intValue();
    }

    @Override
    public long getLong(int columnIndex) throws SQLException {
        return number(columnIndex).longValue();
    }

    @Override
    public float getFloat(int columnIndex) throws SQLException {
        return number(columnIndex).floatValue();
    }

    @Override
    public double getDouble(int columnIndex) throws SQLException {
        return number(columnIndex).doubleValue();
    }

    @Override
    public BigDecimal getBigDecimal(int columnIndex) throws SQLException {
        Object value = value(columnIndex);
        if (value == null) {
            return null;
        }
        if (value instanceof Long whole) {
            return BigDecimal.valueOf(whole);
        }
        if (value instanceof Double real) {
            return BigDecimal.valueOf(real);
        }
        try {
            return new BigDecimal(value.toString().trim());
        } catch (NumberFormatException notNumber) {
            throw new SQLException("column " + columnIndex + " is not a number: " + value, "22018");
        }
    }

    @Override
    @Deprecated
    public BigDecimal getBigDecimal(int columnIndex, int scale) throws SQLException {
        BigDecimal value = getBigDecimal(columnIndex);
        return value == null ? null : value.setScale(scale, RoundingMode.HALF_UP);
    }

    @Override
    public byte[] getBytes(int columnIndex) throws SQLException {
        Object value = value(columnIndex);
        if (value == null) {
            return null;
        }
        if (value instanceof byte[] bytes) {
            return bytes.clone();
        }
        return value.toString().getBytes(StandardCharsets.UTF_8);
    }

    @Override
    public Date getDate(int columnIndex) throws SQLException {
        String text = getString(columnIndex);
        try {
            return text == null ? null : Date.valueOf(text.length() > 10 ? text.substring(0, 10) : text);
        } catch (IllegalArgumentException notDate) {
            throw new SQLException("column " + columnIndex + " is not a date: " + text, "22007");
        }
    }

    @Override
    public Time getTime(int columnIndex) throws SQLException {
        String text = getString(columnIndex);
        try {
            return text == null ? null : Time.valueOf(text.length() > 8 ? text.substring(0, 8) : text);
        } catch (IllegalArgumentException notTime) {
            throw new SQLException("column " + columnIndex + " is not a time: " + text, "22007");
        }
    }

    @Override
    public Timestamp getTimestamp(int columnIndex) throws SQLException {
        String text = getString(columnIndex);
        try {
            return text == null ? null : Timestamp.valueOf(text.replace('T', ' '));
        } catch (IllegalArgumentException notTimestamp) {
            throw new SQLException("column " + columnIndex + " is not a timestamp: " + text, "22007");
        }
    }

    @Override
    public InputStream getBinaryStream(int columnIndex) throws SQLException {
        byte[] bytes = getBytes(columnIndex);
        return bytes == null ? null : new ByteArrayInputStream(bytes);
    }

    @Override
    public InputStream getAsciiStream(int columnIndex) throws SQLException {
        String text = getString(columnIndex);
        return text == null ? null : new ByteArrayInputStream(text.getBytes(StandardCharsets.US_ASCII));
    }

    @Override
    @Deprecated
    public InputStream getUnicodeStream(int columnIndex) throws SQLException {
        String text = getString(columnIndex);
        return text == null ? null : new ByteArrayInputStream(text.getBytes(StandardCharsets.UTF_16BE));
    }

    @Override
    public Reader getCharacterStream(int columnIndex) throws SQLException {
        String text = getString(columnIndex);
        return text == null ? null : new StringReader(text);
    }

    @Override
    public String getNString(int columnIndex) throws SQLException {
        return getString(columnIndex);
    }

    @Override
    public Reader getNCharacterStream(int columnIndex) throws SQLException {
        return getCharacterStream(columnIndex);
    }

    @Override
    public Object getObject(int columnIndex) throws SQLException {
        Object value = value(columnIndex);
        return value instanceof byte[] bytes ? bytes.clone() : value;
    }

    @Override
    public <T> T getObject(int columnIndex, Class<T> type) throws SQLException {
        Object converted;
        if (type == String.class) {
            converted = getString(columnIndex);
        } else if (type == Long.class) {
            converted = getLong(columnIndex);
        } else if (type == Integer.class) {
            converted = getInt(columnIndex);
        } else if (type == Short.class) {
            converted = getShort(columnIndex);
        } else if (type == Byte.class) {
            converted = getByte(columnIndex);
        } else if (type == Double.class) {
            converted = getDouble(columnIndex);
        } else if (type == Float.class) {
            converted = getFloat(columnIndex);
        } else if (type == Boolean.class) {
            converted = getBoolean(columnIndex);
        } else if (type == BigDecimal.class) {
            converted = getBigDecimal(columnIndex);
        } else if (type == byte[].class) {
            converted = getBytes(columnIndex);
        } else if (type == Date.class) {
            converted = getDate(columnIndex);
        } else if (type == Time.class) {
            converted = getTime(columnIndex);
        } else if (type == Timestamp.class) {
            converted = getTimestamp(columnIndex);
        } else if (type == java.time.LocalDate.class) {
            Date date = getDate(columnIndex);
            converted = date == null ? null : date.toLocalDate();
        } else if (type == java.time.LocalDateTime.class) {
            Timestamp timestamp = getTimestamp(columnIndex);
            converted = timestamp == null ? null : timestamp.toLocalDateTime();
        } else if (type == java.util.UUID.class) {
            String text = getString(columnIndex);
            converted = text == null ? null : java.util.UUID.fromString(text);
        } else if (type == Object.class) {
            converted = getObject(columnIndex);
        } else {
            throw unsupported("reading a " + type.getName());
        }
        return wasNull ? null : type.cast(converted);
    }

    @Override
    public Object getObject(int columnIndex, Map<String, Class<?>> map) throws SQLException {
        return getObject(columnIndex);
    }

    @Override
    public SQLWarning getWarnings() {
        return null;
    }

    @Override
    public void clearWarnings() {
    }

    @Override
    public String getCursorName() throws SQLException {
        throw unsupported("named cursors");
    }

    @Override
    public ResultSetMetaData getMetaData() {
        return new EggDbResultSetMetaData(this);
    }

    @Override
    public boolean isBeforeFirst() {
        return row < 0 && !rows.isEmpty();
    }

    @Override
    public boolean isAfterLast() {
        return row >= rows.size() && !rows.isEmpty();
    }

    @Override
    public boolean isFirst() {
        return row == 0 && !rows.isEmpty();
    }

    @Override
    public boolean isLast() {
        return row == rows.size() - 1 && !rows.isEmpty();
    }

    @Override
    public int getRow() {
        return row >= 0 && row < rows.size() ? row + 1 : 0;
    }

    @Override
    public void setFetchDirection(int direction) throws SQLException {
        if (direction != FETCH_FORWARD) {
            throw unsupported("fetching backwards");
        }
    }

    @Override
    public int getFetchDirection() {
        return FETCH_FORWARD;
    }

    @Override
    public void setFetchSize(int rows) {
    }

    @Override
    public int getFetchSize() {
        return 0;
    }

    @Override
    public int getType() {
        return TYPE_FORWARD_ONLY;
    }

    @Override
    public int getConcurrency() {
        return CONCUR_READ_ONLY;
    }

    @Override
    public int getHoldability() {
        return CLOSE_CURSORS_AT_COMMIT;
    }

    @Override
    public Statement getStatement() {
        return statement;
    }

    @Override
    public boolean rowUpdated() {
        return false;
    }

    @Override
    public boolean rowInserted() {
        return false;
    }

    @Override
    public boolean rowDeleted() {
        return false;
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

    @Override
    public String getString(String columnLabel) throws SQLException {
        return getString(findColumn(columnLabel));
    }

    @Override
    public boolean getBoolean(String columnLabel) throws SQLException {
        return getBoolean(findColumn(columnLabel));
    }

    @Override
    public byte getByte(String columnLabel) throws SQLException {
        return getByte(findColumn(columnLabel));
    }

    @Override
    public short getShort(String columnLabel) throws SQLException {
        return getShort(findColumn(columnLabel));
    }

    @Override
    public int getInt(String columnLabel) throws SQLException {
        return getInt(findColumn(columnLabel));
    }

    @Override
    public long getLong(String columnLabel) throws SQLException {
        return getLong(findColumn(columnLabel));
    }

    @Override
    public float getFloat(String columnLabel) throws SQLException {
        return getFloat(findColumn(columnLabel));
    }

    @Override
    public double getDouble(String columnLabel) throws SQLException {
        return getDouble(findColumn(columnLabel));
    }

    @Override
    public BigDecimal getBigDecimal(String columnLabel) throws SQLException {
        return getBigDecimal(findColumn(columnLabel));
    }

    @Override
    public byte[] getBytes(String columnLabel) throws SQLException {
        return getBytes(findColumn(columnLabel));
    }

    @Override
    public Date getDate(String columnLabel) throws SQLException {
        return getDate(findColumn(columnLabel));
    }

    @Override
    public Time getTime(String columnLabel) throws SQLException {
        return getTime(findColumn(columnLabel));
    }

    @Override
    public Timestamp getTimestamp(String columnLabel) throws SQLException {
        return getTimestamp(findColumn(columnLabel));
    }

    @Override
    public InputStream getAsciiStream(String columnLabel) throws SQLException {
        return getAsciiStream(findColumn(columnLabel));
    }

    @Override
    public InputStream getBinaryStream(String columnLabel) throws SQLException {
        return getBinaryStream(findColumn(columnLabel));
    }

    @Override
    public Reader getCharacterStream(String columnLabel) throws SQLException {
        return getCharacterStream(findColumn(columnLabel));
    }

    @Override
    public Object getObject(String columnLabel) throws SQLException {
        return getObject(findColumn(columnLabel));
    }

    @Override
    public String getNString(String columnLabel) throws SQLException {
        return getNString(findColumn(columnLabel));
    }

    @Override
    public Reader getNCharacterStream(String columnLabel) throws SQLException {
        return getNCharacterStream(findColumn(columnLabel));
    }

    @Override
    @Deprecated
    public BigDecimal getBigDecimal(String columnLabel, int scale) throws SQLException {
        return getBigDecimal(findColumn(columnLabel), scale);
    }

    @Override
    @Deprecated
    public InputStream getUnicodeStream(String columnLabel) throws SQLException {
        return getUnicodeStream(findColumn(columnLabel));
    }

    @Override
    public <T> T getObject(String columnLabel, Class<T> type) throws SQLException {
        return getObject(findColumn(columnLabel), type);
    }

    @Override
    public Object getObject(String columnLabel, Map<String, Class<?>> map) throws SQLException {
        return getObject(findColumn(columnLabel));
    }

    @Override
    public Date getDate(int columnIndex, Calendar calendar) throws SQLException {
        return getDate(columnIndex);
    }

    @Override
    public Date getDate(String columnLabel, Calendar calendar) throws SQLException {
        return getDate(findColumn(columnLabel));
    }

    @Override
    public Time getTime(int columnIndex, Calendar calendar) throws SQLException {
        return getTime(columnIndex);
    }

    @Override
    public Time getTime(String columnLabel, Calendar calendar) throws SQLException {
        return getTime(findColumn(columnLabel));
    }

    @Override
    public Timestamp getTimestamp(int columnIndex, Calendar calendar) throws SQLException {
        return getTimestamp(columnIndex);
    }

    @Override
    public Timestamp getTimestamp(String columnLabel, Calendar calendar) throws SQLException {
        return getTimestamp(findColumn(columnLabel));
    }

    @Override
    public Ref getRef(int columnIndex) throws SQLException {
        throw unsupported("getRef");
    }

    @Override
    public Ref getRef(String columnLabel) throws SQLException {
        throw unsupported("getRef");
    }

    @Override
    public Blob getBlob(int columnIndex) throws SQLException {
        throw unsupported("getBlob");
    }

    @Override
    public Blob getBlob(String columnLabel) throws SQLException {
        throw unsupported("getBlob");
    }

    @Override
    public Clob getClob(int columnIndex) throws SQLException {
        throw unsupported("getClob");
    }

    @Override
    public Clob getClob(String columnLabel) throws SQLException {
        throw unsupported("getClob");
    }

    @Override
    public Array getArray(int columnIndex) throws SQLException {
        throw unsupported("getArray");
    }

    @Override
    public Array getArray(String columnLabel) throws SQLException {
        throw unsupported("getArray");
    }

    @Override
    public URL getURL(int columnIndex) throws SQLException {
        throw unsupported("getURL");
    }

    @Override
    public URL getURL(String columnLabel) throws SQLException {
        throw unsupported("getURL");
    }

    @Override
    public RowId getRowId(int columnIndex) throws SQLException {
        throw unsupported("getRowId");
    }

    @Override
    public RowId getRowId(String columnLabel) throws SQLException {
        throw unsupported("getRowId");
    }

    @Override
    public NClob getNClob(int columnIndex) throws SQLException {
        throw unsupported("getNClob");
    }

    @Override
    public NClob getNClob(String columnLabel) throws SQLException {
        throw unsupported("getNClob");
    }

    @Override
    public SQLXML getSQLXML(int columnIndex) throws SQLException {
        throw unsupported("getSQLXML");
    }

    @Override
    public SQLXML getSQLXML(String columnLabel) throws SQLException {
        throw unsupported("getSQLXML");
    }

    @Override
    public void beforeFirst() throws SQLException {
        throw unsupported("beforeFirst on a forward-only, read-only result set");
    }

    @Override
    public void afterLast() throws SQLException {
        throw unsupported("afterLast on a forward-only, read-only result set");
    }

    @Override
    public boolean first() throws SQLException {
        throw unsupported("first on a forward-only, read-only result set");
    }

    @Override
    public boolean last() throws SQLException {
        throw unsupported("last on a forward-only, read-only result set");
    }

    @Override
    public boolean absolute(int row) throws SQLException {
        throw unsupported("absolute on a forward-only, read-only result set");
    }

    @Override
    public boolean relative(int rows) throws SQLException {
        throw unsupported("relative on a forward-only, read-only result set");
    }

    @Override
    public boolean previous() throws SQLException {
        throw unsupported("previous on a forward-only, read-only result set");
    }

    @Override
    public void insertRow() throws SQLException {
        throw unsupported("insertRow on a forward-only, read-only result set");
    }

    @Override
    public void updateRow() throws SQLException {
        throw unsupported("updateRow on a forward-only, read-only result set");
    }

    @Override
    public void deleteRow() throws SQLException {
        throw unsupported("deleteRow on a forward-only, read-only result set");
    }

    @Override
    public void refreshRow() throws SQLException {
        throw unsupported("refreshRow on a forward-only, read-only result set");
    }

    @Override
    public void cancelRowUpdates() throws SQLException {
        throw unsupported("cancelRowUpdates on a forward-only, read-only result set");
    }

    @Override
    public void moveToInsertRow() throws SQLException {
        throw unsupported("moveToInsertRow on a forward-only, read-only result set");
    }

    @Override
    public void moveToCurrentRow() throws SQLException {
        throw unsupported("moveToCurrentRow on a forward-only, read-only result set");
    }

    @Override
    public void updateNull(int columnIndex) throws SQLException {
        throw unsupported("updateNull on a read-only result set");
    }

    @Override
    public void updateNull(String columnLabel) throws SQLException {
        throw unsupported("updateNull on a read-only result set");
    }

    @Override
    public void updateBoolean(int columnIndex, boolean x) throws SQLException {
        throw unsupported("updateBoolean on a read-only result set");
    }

    @Override
    public void updateBoolean(String columnLabel, boolean x) throws SQLException {
        throw unsupported("updateBoolean on a read-only result set");
    }

    @Override
    public void updateByte(int columnIndex, byte x) throws SQLException {
        throw unsupported("updateByte on a read-only result set");
    }

    @Override
    public void updateByte(String columnLabel, byte x) throws SQLException {
        throw unsupported("updateByte on a read-only result set");
    }

    @Override
    public void updateShort(int columnIndex, short x) throws SQLException {
        throw unsupported("updateShort on a read-only result set");
    }

    @Override
    public void updateShort(String columnLabel, short x) throws SQLException {
        throw unsupported("updateShort on a read-only result set");
    }

    @Override
    public void updateInt(int columnIndex, int x) throws SQLException {
        throw unsupported("updateInt on a read-only result set");
    }

    @Override
    public void updateInt(String columnLabel, int x) throws SQLException {
        throw unsupported("updateInt on a read-only result set");
    }

    @Override
    public void updateLong(int columnIndex, long x) throws SQLException {
        throw unsupported("updateLong on a read-only result set");
    }

    @Override
    public void updateLong(String columnLabel, long x) throws SQLException {
        throw unsupported("updateLong on a read-only result set");
    }

    @Override
    public void updateFloat(int columnIndex, float x) throws SQLException {
        throw unsupported("updateFloat on a read-only result set");
    }

    @Override
    public void updateFloat(String columnLabel, float x) throws SQLException {
        throw unsupported("updateFloat on a read-only result set");
    }

    @Override
    public void updateDouble(int columnIndex, double x) throws SQLException {
        throw unsupported("updateDouble on a read-only result set");
    }

    @Override
    public void updateDouble(String columnLabel, double x) throws SQLException {
        throw unsupported("updateDouble on a read-only result set");
    }

    @Override
    public void updateBigDecimal(int columnIndex, BigDecimal x) throws SQLException {
        throw unsupported("updateBigDecimal on a read-only result set");
    }

    @Override
    public void updateBigDecimal(String columnLabel, BigDecimal x) throws SQLException {
        throw unsupported("updateBigDecimal on a read-only result set");
    }

    @Override
    public void updateString(int columnIndex, String x) throws SQLException {
        throw unsupported("updateString on a read-only result set");
    }

    @Override
    public void updateString(String columnLabel, String x) throws SQLException {
        throw unsupported("updateString on a read-only result set");
    }

    @Override
    public void updateBytes(int columnIndex, byte[] x) throws SQLException {
        throw unsupported("updateBytes on a read-only result set");
    }

    @Override
    public void updateBytes(String columnLabel, byte[] x) throws SQLException {
        throw unsupported("updateBytes on a read-only result set");
    }

    @Override
    public void updateDate(int columnIndex, Date x) throws SQLException {
        throw unsupported("updateDate on a read-only result set");
    }

    @Override
    public void updateDate(String columnLabel, Date x) throws SQLException {
        throw unsupported("updateDate on a read-only result set");
    }

    @Override
    public void updateTime(int columnIndex, Time x) throws SQLException {
        throw unsupported("updateTime on a read-only result set");
    }

    @Override
    public void updateTime(String columnLabel, Time x) throws SQLException {
        throw unsupported("updateTime on a read-only result set");
    }

    @Override
    public void updateTimestamp(int columnIndex, Timestamp x) throws SQLException {
        throw unsupported("updateTimestamp on a read-only result set");
    }

    @Override
    public void updateTimestamp(String columnLabel, Timestamp x) throws SQLException {
        throw unsupported("updateTimestamp on a read-only result set");
    }

    @Override
    public void updateAsciiStream(int columnIndex, InputStream x, int length) throws SQLException {
        throw unsupported("updateAsciiStream on a read-only result set");
    }

    @Override
    public void updateAsciiStream(String columnLabel, InputStream x, int length) throws SQLException {
        throw unsupported("updateAsciiStream on a read-only result set");
    }

    @Override
    public void updateBinaryStream(int columnIndex, InputStream x, int length) throws SQLException {
        throw unsupported("updateBinaryStream on a read-only result set");
    }

    @Override
    public void updateBinaryStream(String columnLabel, InputStream x, int length) throws SQLException {
        throw unsupported("updateBinaryStream on a read-only result set");
    }

    @Override
    public void updateCharacterStream(int columnIndex, Reader x, int length) throws SQLException {
        throw unsupported("updateCharacterStream on a read-only result set");
    }

    @Override
    public void updateCharacterStream(String columnLabel, Reader x, int length) throws SQLException {
        throw unsupported("updateCharacterStream on a read-only result set");
    }

    @Override
    public void updateObject(int columnIndex, Object x, int scaleOrLength) throws SQLException {
        throw unsupported("updateObject on a read-only result set");
    }

    @Override
    public void updateObject(String columnLabel, Object x, int scaleOrLength) throws SQLException {
        throw unsupported("updateObject on a read-only result set");
    }

    @Override
    public void updateObject(int columnIndex, Object x) throws SQLException {
        throw unsupported("updateObject on a read-only result set");
    }

    @Override
    public void updateObject(String columnLabel, Object x) throws SQLException {
        throw unsupported("updateObject on a read-only result set");
    }

    @Override
    public void updateRef(int columnIndex, Ref x) throws SQLException {
        throw unsupported("updateRef on a read-only result set");
    }

    @Override
    public void updateRef(String columnLabel, Ref x) throws SQLException {
        throw unsupported("updateRef on a read-only result set");
    }

    @Override
    public void updateBlob(int columnIndex, Blob x) throws SQLException {
        throw unsupported("updateBlob on a read-only result set");
    }

    @Override
    public void updateBlob(String columnLabel, Blob x) throws SQLException {
        throw unsupported("updateBlob on a read-only result set");
    }

    @Override
    public void updateClob(int columnIndex, Clob x) throws SQLException {
        throw unsupported("updateClob on a read-only result set");
    }

    @Override
    public void updateClob(String columnLabel, Clob x) throws SQLException {
        throw unsupported("updateClob on a read-only result set");
    }

    @Override
    public void updateArray(int columnIndex, Array x) throws SQLException {
        throw unsupported("updateArray on a read-only result set");
    }

    @Override
    public void updateArray(String columnLabel, Array x) throws SQLException {
        throw unsupported("updateArray on a read-only result set");
    }

    @Override
    public void updateRowId(int columnIndex, RowId x) throws SQLException {
        throw unsupported("updateRowId on a read-only result set");
    }

    @Override
    public void updateRowId(String columnLabel, RowId x) throws SQLException {
        throw unsupported("updateRowId on a read-only result set");
    }

    @Override
    public void updateNString(int columnIndex, String nString) throws SQLException {
        throw unsupported("updateNString on a read-only result set");
    }

    @Override
    public void updateNString(String columnLabel, String nString) throws SQLException {
        throw unsupported("updateNString on a read-only result set");
    }

    @Override
    public void updateNClob(int columnIndex, NClob nClob) throws SQLException {
        throw unsupported("updateNClob on a read-only result set");
    }

    @Override
    public void updateNClob(String columnLabel, NClob nClob) throws SQLException {
        throw unsupported("updateNClob on a read-only result set");
    }

    @Override
    public void updateSQLXML(int columnIndex, SQLXML xmlObject) throws SQLException {
        throw unsupported("updateSQLXML on a read-only result set");
    }

    @Override
    public void updateSQLXML(String columnLabel, SQLXML xmlObject) throws SQLException {
        throw unsupported("updateSQLXML on a read-only result set");
    }

    @Override
    public void updateNCharacterStream(int columnIndex, Reader x, long length) throws SQLException {
        throw unsupported("updateNCharacterStream on a read-only result set");
    }

    @Override
    public void updateNCharacterStream(String columnLabel, Reader x, long length) throws SQLException {
        throw unsupported("updateNCharacterStream on a read-only result set");
    }

    @Override
    public void updateAsciiStream(int columnIndex, InputStream x, long length) throws SQLException {
        throw unsupported("updateAsciiStream on a read-only result set");
    }

    @Override
    public void updateAsciiStream(String columnLabel, InputStream x, long length) throws SQLException {
        throw unsupported("updateAsciiStream on a read-only result set");
    }

    @Override
    public void updateBinaryStream(int columnIndex, InputStream x, long length) throws SQLException {
        throw unsupported("updateBinaryStream on a read-only result set");
    }

    @Override
    public void updateBinaryStream(String columnLabel, InputStream x, long length) throws SQLException {
        throw unsupported("updateBinaryStream on a read-only result set");
    }

    @Override
    public void updateCharacterStream(int columnIndex, Reader x, long length) throws SQLException {
        throw unsupported("updateCharacterStream on a read-only result set");
    }

    @Override
    public void updateCharacterStream(String columnLabel, Reader x, long length) throws SQLException {
        throw unsupported("updateCharacterStream on a read-only result set");
    }

    @Override
    public void updateBlob(int columnIndex, InputStream inputStream, long length) throws SQLException {
        throw unsupported("updateBlob on a read-only result set");
    }

    @Override
    public void updateBlob(String columnLabel, InputStream inputStream, long length) throws SQLException {
        throw unsupported("updateBlob on a read-only result set");
    }

    @Override
    public void updateClob(int columnIndex, Reader reader, long length) throws SQLException {
        throw unsupported("updateClob on a read-only result set");
    }

    @Override
    public void updateClob(String columnLabel, Reader reader, long length) throws SQLException {
        throw unsupported("updateClob on a read-only result set");
    }

    @Override
    public void updateNClob(int columnIndex, Reader reader, long length) throws SQLException {
        throw unsupported("updateNClob on a read-only result set");
    }

    @Override
    public void updateNClob(String columnLabel, Reader reader, long length) throws SQLException {
        throw unsupported("updateNClob on a read-only result set");
    }

    @Override
    public void updateNCharacterStream(int columnIndex, Reader x) throws SQLException {
        throw unsupported("updateNCharacterStream on a read-only result set");
    }

    @Override
    public void updateNCharacterStream(String columnLabel, Reader x) throws SQLException {
        throw unsupported("updateNCharacterStream on a read-only result set");
    }

    @Override
    public void updateAsciiStream(int columnIndex, InputStream x) throws SQLException {
        throw unsupported("updateAsciiStream on a read-only result set");
    }

    @Override
    public void updateAsciiStream(String columnLabel, InputStream x) throws SQLException {
        throw unsupported("updateAsciiStream on a read-only result set");
    }

    @Override
    public void updateBinaryStream(int columnIndex, InputStream x) throws SQLException {
        throw unsupported("updateBinaryStream on a read-only result set");
    }

    @Override
    public void updateBinaryStream(String columnLabel, InputStream x) throws SQLException {
        throw unsupported("updateBinaryStream on a read-only result set");
    }

    @Override
    public void updateCharacterStream(int columnIndex, Reader x) throws SQLException {
        throw unsupported("updateCharacterStream on a read-only result set");
    }

    @Override
    public void updateCharacterStream(String columnLabel, Reader x) throws SQLException {
        throw unsupported("updateCharacterStream on a read-only result set");
    }

    @Override
    public void updateBlob(int columnIndex, InputStream inputStream) throws SQLException {
        throw unsupported("updateBlob on a read-only result set");
    }

    @Override
    public void updateBlob(String columnLabel, InputStream inputStream) throws SQLException {
        throw unsupported("updateBlob on a read-only result set");
    }

    @Override
    public void updateClob(int columnIndex, Reader reader) throws SQLException {
        throw unsupported("updateClob on a read-only result set");
    }

    @Override
    public void updateClob(String columnLabel, Reader reader) throws SQLException {
        throw unsupported("updateClob on a read-only result set");
    }

    @Override
    public void updateNClob(int columnIndex, Reader reader) throws SQLException {
        throw unsupported("updateNClob on a read-only result set");
    }

    @Override
    public void updateNClob(String columnLabel, Reader reader) throws SQLException {
        throw unsupported("updateNClob on a read-only result set");
    }
}

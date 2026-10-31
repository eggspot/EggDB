package app.eggspot.eggdb;

import java.sql.ResultSetMetaData;
import java.sql.SQLException;
import java.sql.Types;

/** The columns of an {@link EggDbResultSet}, with types taken from the values each column holds. */
public final class EggDbResultSetMetaData implements ResultSetMetaData {
    private final EggDbResultSet resultSet;

    EggDbResultSetMetaData(EggDbResultSet resultSet) {
        this.resultSet = resultSet;
    }

    private Object firstValue(int column) throws SQLException {
        if (column < 1 || column > resultSet.columns().size()) {
            throw new SQLException("no column " + column, "07009");
        }
        for (Object[] row : resultSet.rows()) {
            if (row[column - 1] != null) {
                return row[column - 1];
            }
        }
        return null;
    }

    @Override
    public int getColumnCount() {
        return resultSet.columns().size();
    }

    @Override
    public boolean isAutoIncrement(int column) {
        return false;
    }

    @Override
    public boolean isCaseSensitive(int column) throws SQLException {
        return firstValue(column) instanceof String;
    }

    @Override
    public boolean isSearchable(int column) {
        return true;
    }

    @Override
    public boolean isCurrency(int column) {
        return false;
    }

    @Override
    public int isNullable(int column) {
        return columnNullableUnknown;
    }

    @Override
    public boolean isSigned(int column) throws SQLException {
        return firstValue(column) instanceof Number;
    }

    @Override
    public int getColumnDisplaySize(int column) {
        return 0;
    }

    @Override
    public String getColumnLabel(int column) throws SQLException {
        return getColumnName(column);
    }

    @Override
    public String getColumnName(int column) throws SQLException {
        if (column < 1 || column > resultSet.columns().size()) {
            throw new SQLException("no column " + column, "07009");
        }
        return resultSet.columns().get(column - 1);
    }

    @Override
    public String getSchemaName(int column) {
        return "";
    }

    @Override
    public int getPrecision(int column) {
        return 0;
    }

    @Override
    public int getScale(int column) {
        return 0;
    }

    @Override
    public String getTableName(int column) {
        return "";
    }

    @Override
    public String getCatalogName(int column) {
        return "";
    }

    @Override
    public int getColumnType(int column) throws SQLException {
        Object value = firstValue(column);
        if (value instanceof Long) {
            return Types.BIGINT;
        }
        if (value instanceof Double) {
            return Types.DOUBLE;
        }
        if (value instanceof byte[]) {
            return Types.VARBINARY;
        }
        if (value instanceof String) {
            return Types.VARCHAR;
        }
        return Types.NULL;
    }

    @Override
    public String getColumnTypeName(int column) throws SQLException {
        return switch (getColumnType(column)) {
            case Types.BIGINT -> "INTEGER";
            case Types.DOUBLE -> "REAL";
            case Types.VARBINARY -> "BLOB";
            case Types.VARCHAR -> "TEXT";
            default -> "NULL";
        };
    }

    @Override
    public boolean isReadOnly(int column) {
        return true;
    }

    @Override
    public boolean isWritable(int column) {
        return false;
    }

    @Override
    public boolean isDefinitelyWritable(int column) {
        return false;
    }

    @Override
    public String getColumnClassName(int column) throws SQLException {
        return switch (getColumnType(column)) {
            case Types.BIGINT -> Long.class.getName();
            case Types.DOUBLE -> Double.class.getName();
            case Types.VARBINARY -> byte[].class.getName();
            case Types.VARCHAR -> String.class.getName();
            default -> Object.class.getName();
        };
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

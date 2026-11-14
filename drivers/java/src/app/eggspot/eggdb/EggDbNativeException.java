package app.eggspot.eggdb;

import java.sql.SQLException;

final class EggDbNativeException extends RuntimeException {
    private static final long serialVersionUID = 1L;

    private final String sqlState;
    private final int status;

    EggDbNativeException(String message, String sqlState, int status) {
        super(message);
        this.sqlState = sqlState == null || sqlState.isEmpty() ? null : sqlState;
        this.status = status;
    }

    SQLException asSqlException() {
        return new SQLException(getMessage(), sqlState, status);
    }
}

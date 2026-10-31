package app.eggspot.eggdb;

import java.math.BigDecimal;
import java.util.HexFormat;

final class Parameters {
    private Parameters() {
    }

    static String text(Object value, String dialect) {
        if (value == null) {
            return null;
        }
        if (value instanceof Boolean flag) {
            if (dialect.equals("postgresql")) {
                return flag ? "true" : "false";
            }
            return flag ? "1" : "0";
        }
        if (value instanceof BigDecimal decimal) {
            return decimal.toPlainString();
        }
        if (value instanceof byte[] bytes) {
            return "\\x" + HexFormat.of().formatHex(bytes);
        }
        return value.toString();
    }

    static String numbered(String sql, String dialect) {
        if (!dialect.equals("postgresql")) {
            return sql;
        }
        StringBuilder out = new StringBuilder(sql.length() + 8);
        char quote = 0;
        int number = 0;
        for (int index = 0; index < sql.length(); index++) {
            char character = sql.charAt(index);
            if (quote != 0) {
                if (character == quote) {
                    quote = 0;
                }
                out.append(character);
            } else if (character == '\'' || character == '"') {
                quote = character;
                out.append(character);
            } else if (character == '?') {
                out.append('$').append(++number);
            } else {
                out.append(character);
            }
        }
        return out.toString();
    }
}

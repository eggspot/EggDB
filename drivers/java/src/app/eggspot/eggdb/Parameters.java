package app.eggspot.eggdb;

import java.math.BigDecimal;
import java.util.ArrayList;
import java.util.HexFormat;
import java.util.List;

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

    record Bound(String sql, List<Object> parameters) {
    }

    static Bound bound(String sql, String dialect, List<Object> parameters) {
        boolean postgresql = dialect.equals("postgresql");
        List<Object> kept = new ArrayList<>(parameters.size());
        StringBuilder out = new StringBuilder(sql.length() + 8);
        char quote = 0;
        int given = 0;
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
                Object value = given < parameters.size() ? parameters.get(given) : null;
                given++;
                if (!postgresql && value instanceof byte[] blob) {
                    out.append("X'").append(HexFormat.of().formatHex(blob)).append('\'');
                    continue;
                }
                kept.add(value);
                if (postgresql) {
                    out.append('$').append(kept.size());
                } else {
                    out.append('?');
                }
            } else {
                out.append(character);
            }
        }
        if (given < parameters.size()) {
            kept.addAll(parameters.subList(given, parameters.size()));
        }
        return new Bound(out.toString(), kept);
    }
}

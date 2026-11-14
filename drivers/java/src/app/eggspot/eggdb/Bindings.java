package app.eggspot.eggdb;

import java.math.BigDecimal;
import java.math.BigInteger;
import java.nio.charset.StandardCharsets;
import java.util.List;

record Bindings(int[] kinds, long[] integers, double[] reals, byte[][] bytes) {
    static final int NULL = 0;
    static final int INTEGER = 1;
    static final int REAL = 2;
    static final int TEXT = 3;
    static final int BLOB = 4;
    static final int BOOLEAN = 5;

    private static final BigInteger LONG_MIN = BigInteger.valueOf(Long.MIN_VALUE);
    private static final BigInteger LONG_MAX = BigInteger.valueOf(Long.MAX_VALUE);

    static Bindings of(List<Object> values, String dialect) {
        int count = values.size();
        Bindings bindings = new Bindings(new int[count], new long[count], new double[count], new byte[count][]);
        for (int at = 0; at < count; at++) {
            bindings.set(at, values.get(at), dialect);
        }
        return bindings;
    }

    private void set(int at, Object value, String dialect) {
        if (value == null) {
            kinds[at] = NULL;
        } else if (value instanceof Boolean flag) {
            kinds[at] = BOOLEAN;
            integers[at] = flag ? 1 : 0;
        } else if (value instanceof Long || value instanceof Integer || value instanceof Short || value instanceof Byte) {
            kinds[at] = INTEGER;
            integers[at] = ((Number) value).longValue();
        } else if (value instanceof BigInteger big && big.compareTo(LONG_MIN) >= 0 && big.compareTo(LONG_MAX) <= 0) {
            kinds[at] = INTEGER;
            integers[at] = big.longValue();
        } else if (value instanceof Double || value instanceof Float) {
            kinds[at] = REAL;
            reals[at] = ((Number) value).doubleValue();
        } else if (value instanceof byte[] blob) {
            kinds[at] = BLOB;
            bytes[at] = blob;
        } else {
            kinds[at] = TEXT;
            String text = value instanceof BigDecimal decimal ? decimal.toPlainString() : Parameters.text(value, dialect);
            bytes[at] = text.getBytes(StandardCharsets.UTF_8);
        }
    }

    int size() {
        return kinds.length;
    }
}

<?php

declare(strict_types=1);

namespace EggDB;

final class Connection
{
    private const DECLARATIONS = <<<'C'
typedef struct eggdb_db eggdb_db;
typedef struct eggdb_sql eggdb_sql;
typedef struct eggdb_options { uint32_t flags; uint32_t durability; uint64_t cache_bytes; } eggdb_options;
typedef struct eggdb_value { int32_t kind; int64_t integer; double real; const uint8_t *data; size_t len; } eggdb_value;
typedef struct eggdb_parameter { const uint8_t *text; size_t len; } eggdb_parameter;
typedef struct eggdb_result { const char *tag; size_t column_count; const char *const *column_names; size_t row_count; const eggdb_value *values; } eggdb_result;
const char *eggdb_last_error(void);
const char *eggdb_last_sqlstate(void);
int eggdb_open(const char *path, const eggdb_options *options, eggdb_db **db);
int eggdb_close(eggdb_db *db);
int eggdb_sql_open(eggdb_db *db, const char *dialect, eggdb_sql **sql);
int eggdb_sql_connect(const char *url, eggdb_sql **sql);
int eggdb_sql_execute_with_parameters(eggdb_sql *sql, const char *text, const eggdb_parameter *parameters, size_t parameter_count, const eggdb_result **results, size_t *count);
int eggdb_sql_close(eggdb_sql *sql);
typedef struct eggdb_stmt eggdb_stmt;
typedef struct eggdb_copy eggdb_copy;
int eggdb_sql_prepare(eggdb_sql *sql, const char *text, size_t len, eggdb_stmt **stmt);
int eggdb_stmt_parameter_count(eggdb_stmt *stmt, size_t *count);
int eggdb_stmt_bind_null(eggdb_stmt *stmt, size_t index);
int eggdb_stmt_bind_int64(eggdb_stmt *stmt, size_t index, int64_t value);
int eggdb_stmt_bind_double(eggdb_stmt *stmt, size_t index, double value);
int eggdb_stmt_bind_bool(eggdb_stmt *stmt, size_t index, int value);
int eggdb_stmt_bind_text(eggdb_stmt *stmt, size_t index, const char *text, size_t len);
int eggdb_stmt_bind_blob(eggdb_stmt *stmt, size_t index, const uint8_t *data, size_t len);
int eggdb_stmt_clear_bindings(eggdb_stmt *stmt);
int eggdb_stmt_execute(eggdb_stmt *stmt, const eggdb_result **results, size_t *count);
int eggdb_stmt_reset(eggdb_stmt *stmt);
int eggdb_stmt_column_count(eggdb_stmt *stmt, size_t *count);
int eggdb_stmt_column_name(eggdb_stmt *stmt, size_t column, const char **name);
int eggdb_stmt_step(eggdb_stmt *stmt, int *has_row);
int eggdb_stmt_row(eggdb_stmt *stmt, const eggdb_value **values, size_t *count);
int eggdb_stmt_finalize(eggdb_stmt *stmt);
int eggdb_sql_copy_in_begin(eggdb_sql *sql, const char *text, eggdb_copy **copy);
int eggdb_sql_copy_out_begin(eggdb_sql *sql, const char *text, eggdb_copy **copy);
int eggdb_copy_put(eggdb_copy *copy, const uint8_t *data, size_t len);
int eggdb_copy_read(eggdb_copy *copy, const uint8_t **data, size_t *len);
int eggdb_copy_end(eggdb_copy *copy, const eggdb_result **results, size_t *count);
void eggdb_copy_free(eggdb_copy *copy);
C;

    private \FFI $ffi;
    private $database = null;
    private $session;
    private string $dialect;
    private array $statements = [];

    public function __construct(string $pathOrUrl, string $dialect = 'sqlite', ?string $library = null)
    {
        $library ??= getenv('EGGDB_LIBRARY') ?: (PHP_OS_FAMILY === 'Darwin' ? 'libeggdb_capi.dylib' : 'libeggdb_capi.so');
        $this->ffi = \FFI::cdef(self::DECLARATIONS, $library);
        $session = $this->ffi->new('eggdb_sql*');
        if (str_starts_with($pathOrUrl, 'postgresql://') || str_starts_with($pathOrUrl, 'postgres://')) {
            $this->check($this->ffi->eggdb_sql_connect($pathOrUrl, \FFI::addr($session)));
            $this->dialect = 'postgresql';
        } else {
            $options = $this->ffi->new('eggdb_options');
            $database = $this->ffi->new('eggdb_db*');
            $this->check($this->ffi->eggdb_open($pathOrUrl, \FFI::addr($options), \FFI::addr($database)));
            $this->database = $database;
            $status = $this->ffi->eggdb_sql_open($database, $dialect, \FFI::addr($session));
            if ($status !== 0) {
                $this->ffi->eggdb_close($database);
                $this->check($status);
            }
            $this->dialect = $dialect;
        }
        $this->session = $session;
    }

    private function check(int $status): void
    {
        if ($status === 0) {
            return;
        }
        $message = (string) $this->ffi->eggdb_last_error();
        $state = (string) $this->ffi->eggdb_last_sqlstate();
        throw new EggDbException($message !== '' ? $message : "EggDB status $status", $state, $status);
    }

    private function numbered(string $sql): string
    {
        if ($this->dialect !== 'postgresql') {
            return $sql;
        }
        $out = '';
        $quote = null;
        $number = 0;
        foreach (mb_str_split($sql) as $character) {
            if ($quote !== null) {
                if ($character === $quote) {
                    $quote = null;
                }
                $out .= $character;
            } elseif ($character === "'" || $character === '"') {
                $quote = $character;
                $out .= $character;
            } elseif ($character === '?') {
                $out .= '$' . ++$number;
            } else {
                $out .= $character;
            }
        }
        return $out;
    }

    private function text(mixed $value): ?string
    {
        return match (true) {
            $value === null => null,
            $value === true => $this->dialect === 'postgresql' ? 'true' : '1',
            $value === false => $this->dialect === 'postgresql' ? 'false' : '0',
            default => (string) $value,
        };
    }

    public function execute(string $sql, array $parameters = []): array
    {
        $count = count($parameters);
        $array = $this->ffi->new('eggdb_parameter[' . max($count, 1) . ']');
        $kept = [];
        foreach (array_values($parameters) as $index => $value) {
            $text = $this->text($value);
            if ($text === null) {
                continue;
            }
            $buffer = \FFI::new('uint8_t[' . max(strlen($text), 1) . ']', false);
            \FFI::memcpy($buffer, $text, strlen($text));
            $kept[] = $buffer;
            $array[$index]->text = \FFI::cast('const uint8_t*', \FFI::addr($buffer[0]));
            $array[$index]->len = strlen($text);
        }
        $results = $this->ffi->new('const eggdb_result*');
        $resultCount = $this->ffi->new('size_t');
        $status = $this->ffi->eggdb_sql_execute_with_parameters($this->session, $this->numbered($sql), $array, $count, \FFI::addr($results), \FFI::addr($resultCount));
        foreach ($kept as $buffer) {
            \FFI::free($buffer);
        }
        $this->check($status);
        return self::gathered($results, (int) $resultCount->cdata);
    }

    public static function gathered($results, int $count): array
    {
        $gathered = [];
        for ($index = 0; $index < $count; $index++) {
            $result = $results[$index];
            $columns = [];
            for ($column = 0; $column < $result->column_count; $column++) {
                $columns[] = \FFI::string($result->column_names[$column]);
            }
            $rows = [];
            for ($row = 0; $row < $result->row_count; $row++) {
                $cells = [];
                for ($column = 0; $column < $result->column_count; $column++) {
                    $cells[$columns[$column]] = self::cell($result->values[$row * $result->column_count + $column]);
                }
                $rows[] = $cells;
            }
            $gathered[] = ['tag' => \FFI::string($result->tag), 'columns' => $columns, 'rows' => $rows];
        }
        return $gathered;
    }

    public static function cell($cell): mixed
    {
        return match ($cell->kind) {
            1 => $cell->integer,
            2 => $cell->real,
            3, 4 => \FFI::string($cell->data, $cell->len),
            default => null,
        };
    }

    public function prepare(string $sql): Statement
    {
        $this->live();
        $text = $this->numbered($sql);
        $handle = $this->ffi->new('eggdb_stmt*');
        $this->check($this->ffi->eggdb_sql_prepare($this->session, $text, strlen($text), \FFI::addr($handle)));
        $statement = new Statement($this->ffi, $handle, fn (int $status) => $this->check($status), $this->dialect);
        $this->statements[] = \WeakReference::create($statement);
        return $statement;
    }

    public function iterate(string $sql, array $parameters = []): \Generator
    {
        $statement = $this->prepare($sql);
        try {
            yield from $statement->iterate($parameters);
        } finally {
            $statement->finalize();
        }
    }

    public function copyFrom(string $sql, string|iterable $data): int
    {
        $copy = $this->beginCopy('eggdb_sql_copy_in_begin', $sql);
        try {
            foreach (is_string($data) ? [$data] : $data as $piece) {
                $piece = (string) $piece;
                if ($piece === '') {
                    continue;
                }
                $buffer = \FFI::new('uint8_t[' . strlen($piece) . ']', false);
                \FFI::memcpy($buffer, $piece, strlen($piece));
                $status = $this->ffi->eggdb_copy_put($copy, \FFI::cast('const uint8_t*', \FFI::addr($buffer[0])), strlen($piece));
                \FFI::free($buffer);
                $this->check($status);
            }
            return self::copiedRows($this->endCopy($copy));
        } finally {
            $this->ffi->eggdb_copy_free($copy);
        }
    }

    public function copyTo(string $sql, ?callable $onPiece = null): ?string
    {
        $gathered = $onPiece === null ? '' : null;
        $copy = $this->beginCopy('eggdb_sql_copy_out_begin', $sql);
        try {
            $data = $this->ffi->new('const uint8_t*');
            $length = $this->ffi->new('size_t');
            while (true) {
                $this->check($this->ffi->eggdb_copy_read($copy, \FFI::addr($data), \FFI::addr($length)));
                if ($length->cdata === 0) {
                    break;
                }
                $piece = \FFI::string($data, $length->cdata);
                if ($onPiece === null) {
                    $gathered .= $piece;
                } else {
                    $onPiece($piece);
                }
            }
            $this->endCopy($copy);
        } finally {
            $this->ffi->eggdb_copy_free($copy);
        }
        return $gathered;
    }

    private function live(): void
    {
        if ($this->session === null) {
            throw new EggDbException('the connection is closed', '08003', 0);
        }
    }

    private function beginCopy(string $call, string $sql)
    {
        $this->live();
        $copy = $this->ffi->new('eggdb_copy*');
        $this->check($this->ffi->$call($this->session, $sql, \FFI::addr($copy)));
        return $copy;
    }

    private function endCopy($copy): array
    {
        $results = $this->ffi->new('const eggdb_result*');
        $count = $this->ffi->new('size_t');
        $this->check($this->ffi->eggdb_copy_end($copy, \FFI::addr($results), \FFI::addr($count)));
        return self::gathered($results, (int) $count->cdata);
    }

    private static function copiedRows(array $results): int
    {
        foreach (array_reverse($results) as $result) {
            $words = explode(' ', $result['tag']);
            if (count($words) === 2 && $words[0] === 'COPY' && ctype_digit($words[1])) {
                return (int) $words[1];
            }
        }
        return -1;
    }

    public function query(string $sql, array $parameters = []): array
    {
        foreach (array_reverse($this->execute($sql, $parameters)) as $result) {
            if ($result['columns'] !== []) {
                return $result['rows'];
            }
        }
        return [];
    }

    public function close(): void
    {
        if ($this->session === null) {
            return;
        }
        foreach ($this->statements as $reference) {
            $reference->get()?->finalize();
        }
        $this->statements = [];
        $this->ffi->eggdb_sql_close($this->session);
        if ($this->database !== null) {
            $this->ffi->eggdb_close($this->database);
        }
        $this->session = null;
    }
}

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
C;

    private \FFI $ffi;
    private $database = null;
    private $session;
    private string $dialect;

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
        $gathered = [];
        for ($index = 0; $index < $resultCount->cdata; $index++) {
            $result = $results[$index];
            $columns = [];
            for ($column = 0; $column < $result->column_count; $column++) {
                $columns[] = \FFI::string($result->column_names[$column]);
            }
            $rows = [];
            for ($row = 0; $row < $result->row_count; $row++) {
                $cells = [];
                for ($column = 0; $column < $result->column_count; $column++) {
                    $cell = $result->values[$row * $result->column_count + $column];
                    $cells[$columns[$column]] = match ($cell->kind) {
                        1 => $cell->integer,
                        2 => $cell->real,
                        3, 4 => \FFI::string($cell->data, $cell->len),
                        default => null,
                    };
                }
                $rows[] = $cells;
            }
            $gathered[] = ['tag' => \FFI::string($result->tag), 'columns' => $columns, 'rows' => $rows];
        }
        return $gathered;
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
        $this->ffi->eggdb_sql_close($this->session);
        if ($this->database !== null) {
            $this->ffi->eggdb_close($this->database);
        }
        $this->session = null;
    }
}

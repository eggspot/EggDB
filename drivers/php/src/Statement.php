<?php

declare(strict_types=1);

namespace EggDB;

final class Statement
{
    public readonly int $parameterCount;

    public function __construct(private \FFI $ffi, private $handle, private \Closure $check, private string $dialect)
    {
        $count = $this->ffi->new('size_t');
        ($this->check)($this->ffi->eggdb_stmt_parameter_count($this->handle, \FFI::addr($count)));
        $this->parameterCount = (int) $count->cdata;
    }

    public function bind(array $parameters): self
    {
        $handle = $this->live();
        ($this->check)($this->ffi->eggdb_stmt_reset($handle));
        ($this->check)($this->ffi->eggdb_stmt_clear_bindings($handle));
        foreach (array_values($parameters) as $index => $value) {
            ($this->check)($this->bindOne($handle, $index + 1, $value));
        }
        return $this;
    }

    public function execute(array $parameters = []): array
    {
        $this->bind($parameters);
        $results = $this->ffi->new('const eggdb_result*');
        $count = $this->ffi->new('size_t');
        ($this->check)($this->ffi->eggdb_stmt_execute($this->handle, \FFI::addr($results), \FFI::addr($count)));
        return Connection::gathered($results, (int) $count->cdata);
    }

    public function columns(): array
    {
        $handle = $this->live();
        $count = $this->ffi->new('size_t');
        ($this->check)($this->ffi->eggdb_stmt_column_count($handle, \FFI::addr($count)));
        $names = [];
        $name = $this->ffi->new('const char*');
        for ($column = 0; $column < $count->cdata; $column++) {
            ($this->check)($this->ffi->eggdb_stmt_column_name($handle, $column, \FFI::addr($name)));
            $names[] = \FFI::string($name);
        }
        return $names;
    }

    public function iterate(array $parameters = []): \Generator
    {
        $this->bind($parameters);
        $columns = $this->columns();
        $hasRow = $this->ffi->new('int');
        $values = $this->ffi->new('const eggdb_value*');
        $count = $this->ffi->new('size_t');
        try {
            while (true) {
                ($this->check)($this->ffi->eggdb_stmt_step($this->handle, \FFI::addr($hasRow)));
                if ($hasRow->cdata === 0) {
                    return;
                }
                ($this->check)($this->ffi->eggdb_stmt_row($this->handle, \FFI::addr($values), \FFI::addr($count)));
                $row = [];
                for ($column = 0; $column < $count->cdata; $column++) {
                    $row[$columns[$column] ?? $column] = Connection::cell($values[$column]);
                }
                yield $row;
            }
        } finally {
            if ($this->handle !== null) {
                $this->ffi->eggdb_stmt_reset($this->handle);
            }
        }
    }

    public function finalize(): void
    {
        if ($this->handle !== null) {
            $this->ffi->eggdb_stmt_finalize($this->handle);
            $this->handle = null;
        }
    }

    public function __destruct()
    {
        $this->finalize();
    }

    private function live()
    {
        if ($this->handle === null) {
            throw new EggDbException('the statement is finalized', '26000', 0);
        }
        return $this->handle;
    }

    private function bindOne($handle, int $index, mixed $value): int
    {
        return match (true) {
            $value === null => $this->ffi->eggdb_stmt_bind_null($handle, $index),
            is_bool($value) => $this->ffi->eggdb_stmt_bind_bool($handle, $index, $value ? 1 : 0),
            is_int($value) => $this->ffi->eggdb_stmt_bind_int64($handle, $index, $value),
            is_float($value) => $this->ffi->eggdb_stmt_bind_double($handle, $index, $value),
            is_string($value) && !mb_check_encoding($value, 'UTF-8') => $this->bindBytes($handle, $index, $value),
            default => $this->bindText($handle, $index, $this->text($value)),
        };
    }

    private function text(mixed $value): string
    {
        return $value instanceof \DateTimeInterface ? $value->format('Y-m-d H:i:s.u') : (string) $value;
    }

    private function bindText($handle, int $index, string $text): int
    {
        return $this->ffi->eggdb_stmt_bind_text($handle, $index, $text, strlen($text));
    }

    private function bindBytes($handle, int $index, string $bytes): int
    {
        $buffer = \FFI::new('uint8_t[' . max(strlen($bytes), 1) . ']', false);
        \FFI::memcpy($buffer, $bytes, strlen($bytes));
        $status = $this->ffi->eggdb_stmt_bind_blob($handle, $index, \FFI::cast('const uint8_t*', \FFI::addr($buffer[0])), strlen($bytes));
        \FFI::free($buffer);
        return $status;
    }
}

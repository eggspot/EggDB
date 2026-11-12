<?php

declare(strict_types=1);

namespace EggDB;

final class Transaction
{
    private const SCAN_REVERSE = 1;
    private const SCAN_END_INCLUSIVE = 2;
    private const SCAN_PREFIX = 4;

    public function __construct(private \FFI $ffi, private $handle, public readonly bool $writing)
    {
    }

    public function get(string $key, ?string $collection = null): ?string
    {
        $this->live();
        $data = $this->ffi->new('uint8_t*');
        $length = $this->ffi->new('size_t');
        $this->check($this->ffi->eggdb_get($this->handle, $collection, $key, strlen($key), \FFI::addr($data), \FFI::addr($length)));
        if (\FFI::isNull($data)) {
            return null;
        }
        $value = \FFI::string($data, $length->cdata);
        $this->ffi->eggdb_free($data, $length->cdata);
        return $value;
    }

    public function put(string $key, string $value, ?string $collection = null, ?int $ttlMs = null): void
    {
        $this->live();
        $status = $ttlMs !== null && $ttlMs > 0
            ? $this->ffi->eggdb_put_with_expiry($this->handle, $collection, $key, strlen($key), $value, strlen($value), $ttlMs)
            : $this->ffi->eggdb_put($this->handle, $collection, $key, strlen($key), $value, strlen($value));
        $this->check($status);
    }

    public function delete(string $key, ?string $collection = null): bool
    {
        $this->live();
        $deleted = $this->ffi->new('int');
        $this->check($this->ffi->eggdb_delete($this->handle, $collection, $key, strlen($key), \FFI::addr($deleted)));
        return $deleted->cdata !== 0;
    }

    public function scan(
        ?string $start = null,
        ?string $end = null,
        ?string $prefix = null,
        bool $reverse = false,
        bool $endInclusive = false,
        ?string $collection = null,
    ): \Generator {
        $this->live();
        $flags = ($reverse ? self::SCAN_REVERSE : 0) | ($endInclusive ? self::SCAN_END_INCLUSIVE : 0);
        if ($prefix !== null) {
            $flags |= self::SCAN_PREFIX;
            $start = $prefix;
            $end = null;
        }
        $scan = $this->ffi->new('eggdb_scan*');
        $this->check($this->ffi->eggdb_scan_open(
            $this->handle,
            $collection,
            $start,
            $start === null ? 0 : strlen($start),
            $end,
            $end === null ? 0 : strlen($end),
            $flags,
            \FFI::addr($scan),
        ));
        try {
            $entries = $this->ffi->new('const eggdb_entry*');
            $count = $this->ffi->new('size_t');
            while (true) {
                $this->check($this->ffi->eggdb_scan_next($this->handle, $scan, 0, \FFI::addr($entries), \FFI::addr($count)));
                $total = $count->cdata;
                if ($total === 0) {
                    return;
                }
                $batch = [];
                for ($index = 0; $index < $total; $index++) {
                    $entry = $entries[$index];
                    $batch[] = [\FFI::string($entry->key, $entry->key_len), \FFI::string($entry->value, $entry->value_len)];
                }
                foreach ($batch as [$key, $value]) {
                    yield $key => $value;
                }
            }
        } finally {
            $this->ffi->eggdb_scan_free($scan);
        }
    }

    public function createCollection(string $name): bool
    {
        $this->live();
        $created = $this->ffi->new('int');
        $this->check($this->ffi->eggdb_create_collection($this->handle, $name, \FFI::addr($created)));
        return $created->cdata !== 0;
    }

    public function dropCollection(string $name): bool
    {
        $this->live();
        $dropped = $this->ffi->new('int');
        $this->check($this->ffi->eggdb_drop_collection($this->handle, $name, \FFI::addr($dropped)));
        return $dropped->cdata !== 0;
    }

    public function renameCollection(string $name, string $newName): void
    {
        $this->live();
        $this->check($this->ffi->eggdb_rename_collection($this->handle, $name, $newName));
    }

    public function collections(): array
    {
        $this->live();
        $data = $this->ffi->new('uint8_t*');
        $length = $this->ffi->new('size_t');
        $count = $this->ffi->new('size_t');
        $this->check($this->ffi->eggdb_collection_names($this->handle, \FFI::addr($data), \FFI::addr($length), \FFI::addr($count)));
        if (\FFI::isNull($data)) {
            return [];
        }
        $joined = \FFI::string($data, $length->cdata);
        $this->ffi->eggdb_free($data, $length->cdata);
        return array_slice(explode("\0", $joined), 0, $count->cdata);
    }

    public function collectionStats(string $name): array
    {
        $this->live();
        $stats = $this->ffi->new('eggdb_collection_stats');
        $this->check($this->ffi->eggdb_collection_stats($this->handle, $name, \FFI::addr($stats)));
        return ['entries' => $stats->entries, 'keyBytes' => $stats->key_bytes, 'valueBytes' => $stats->value_bytes];
    }

    public function commit(): int
    {
        $this->live();
        $stamp = $this->ffi->new('uint64_t');
        $handle = $this->handle;
        $this->handle = null;
        $this->check($this->ffi->eggdb_commit($handle, \FFI::addr($stamp)));
        return $stamp->cdata;
    }

    public function rollback(): void
    {
        if ($this->handle === null) {
            return;
        }
        $handle = $this->handle;
        $this->handle = null;
        $this->ffi->eggdb_rollback($handle);
    }

    private function live(): void
    {
        if ($this->handle === null) {
            throw new EggDbException('the transaction has ended', '', 0);
        }
    }

    private function check(int $status): void
    {
        KeyValue::check($this->ffi, $status);
    }
}

package app.eggspot.eggdb;

import java.lang.foreign.Arena;
import java.lang.foreign.FunctionDescriptor;
import java.lang.foreign.Linker;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.SymbolLookup;
import java.lang.invoke.MethodHandle;
import java.nio.file.Path;

final class NativeLibrary {
    private static final Linker LINKER = Linker.nativeLinker();
    private static final SymbolLookup LIBRARY = lookup();

    private NativeLibrary() {
    }

    private static SymbolLookup lookup() {
        String given = System.getProperty("eggdb.library", System.getenv("EGGDB_LIBRARY"));
        if (given != null && !given.isEmpty()) {
            return SymbolLookup.libraryLookup(Path.of(given), Arena.global());
        }
        return SymbolLookup.libraryLookup(System.mapLibraryName("eggdb_capi"), Arena.global());
    }

    static MethodHandle handle(String name, FunctionDescriptor descriptor) {
        MemorySegment symbol = LIBRARY.find(name)
                .orElseThrow(() -> new UnsatisfiedLinkError("the EggDB C library has no " + name));
        return LINKER.downcallHandle(symbol, descriptor);
    }
}

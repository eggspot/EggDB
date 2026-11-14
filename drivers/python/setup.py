import os
import sys

from setuptools import Extension, setup

here = os.path.dirname(os.path.abspath(__file__))
include = os.path.join(here, "..", "..", "crates", "eggdb-capi", "include")

setup(
    name="eggdb",
    version="0.1.0",
    description="EggDB's Python driver: DB-API 2.0 on EggDB's C API, embedded in the process.",
    packages=["eggdb"],
    ext_modules=[
        Extension(
            "eggdb._native",
            sources=["eggdb/_native.c"],
            include_dirs=[include],
            libraries=[] if sys.platform == "win32" else ["dl"] if sys.platform.startswith("linux") else [],
            optional=True,
        )
    ],
)

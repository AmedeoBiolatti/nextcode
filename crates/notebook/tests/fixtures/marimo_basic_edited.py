# /// script
# dependencies = ["marimo"]
# ///

import marimo

__generated_with = "0.25.1"
app = marimo.App()


@app.cell
def _():
    import marimo as mo
    import numpy as np

    return mo, np


@app.cell
def _(mo):
    mo.md(r"""
    # New title
    """)
    return


@app.cell(hide_code=True)
def defs(np):
    x = 2
    z = x * np.e
    return (z,)


@app.cell
def _():
    return


@app.function
def add(a, b):
    return a + b


@app.class_definition
class Point:
    x: int = 0


@app.cell
def _():
    very_long_variable_name_number_one = 1
    very_long_variable_name_number_two = 2
    very_long_variable_name_number_three = 3
    return


@app.cell
def _(y_missing, z):
    print(z, y_missing)
    return


@app.cell(disabled=True)
def _():
    for i in range(3):
        print(i)
    return


app._unparsable_cell(
    r"""
    z = (1,

    """,
    column=None, disabled=False, hide_code=True, expand_output=False, name="_"
)


@app.cell
def _():
    # only comment
    return


@app.cell
def _():
    import os

    return (os,)


@app.function
def helper():
    return 42


@app.cell
def _(os, z_undefined):
    print(add(1, 2), Point, z_undefined, os, helper)
    return


@app.cell
async def _(something):
    async_res = await something()
    return


@app.cell
def _():
    w: int = 5
    return (w,)


@app.cell
def _(w: int):
    print(w)
    return


if __name__ == "__main__":
    app.run()

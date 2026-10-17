import marimo

__generated_with = "0.25.1"
app = marimo.App(width="medium")

with app.setup:
    import marimo as mo
    CONST = 3


@app.function
def uses_const(x):
    return x * CONST


@app.cell
def _():
    mo.md(f"""
        # Hello {CONST}
    """)
    return


@app.cell
def _():
    mo.md("""
    plain *md*
    """)
    return


@app.cell
def _():
    mo.md("""
    Indented
    text with "\"quotes
    """)
    return


@app.cell
def named_md():
    # comment
    mo.md(r"""
    x
    """)  # trailing
    return


@app.cell
def compute():
    result = uses_const(2)
    result
    return


if __name__ == "__main__":
    app.run()

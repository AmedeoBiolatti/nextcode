import marimo

__generated_with = "0.25.1"
app = marimo.App(width="full")


@app.cell
def _():
    import marimo as mo

    return (mo,)


@app.cell
def _(mo):
    mo.md("""
    # Old style
    """)
    return


@app.cell
def _():
    a = 1
    b = 2
    return a, b


@app.cell
def _(a, b):
    c = a + b
    return (c,)


@app.cell
def total(c, mo):
    mo.md(f"Total: {c}")
    return


if __name__ == "__main__":
    app.run()

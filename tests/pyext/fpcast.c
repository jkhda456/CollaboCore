/* A Python extension module whose functions have other types than the pointers the interpreter
   calls them through, as C extensions often declare them (python-xxhash's METH_NOARGS methods take
   only self): natively the extra arguments are ignored, WebAssembly checks the type of an indirect
   call exactly. tests/test_pyext_calls.py builds it as a side module and imports it in the guest;
   the interpreter calls each with the parameters it has (python/patches/cpython-call-trampoline.patch,
   the engine's dylink `params`). */
#define PY_SSIZE_T_CLEAN
#include <Python.h>

/* METH_NOARGS without the unused second parameter */
static PyObject *one_param(PyObject *self) { return PyUnicode_FromString("one param"); }
/* the same, called only in another thread */
static PyObject *in_thread(PyObject *self) { return PyUnicode_FromString("in a thread"); }
/* METH_NOARGS with no parameters at all */
static PyObject *no_params(void) { return PyUnicode_FromString("no params"); }
/* METH_O, as declared */
static PyObject *exact(PyObject *self, PyObject *arg) { return PyNumber_Add(arg, arg); }
/* METH_VARARGS | METH_KEYWORDS without the keywords' parameter */
static PyObject *no_kwargs(PyObject *self, PyObject *args) { return PyLong_FromSsize_t(PyTuple_GET_SIZE(args)); }
/* METH_VARARGS with a keywords' parameter it is never given */
static PyObject *extra_kwargs(PyObject *self, PyObject *args, PyObject *kwargs) {
  return PyUnicode_FromFormat("%zd args, kwargs %s", PyTuple_GET_SIZE(args), kwargs ? "given" : "NULL");
}
/* METH_NOARGS with more parameters than any call has: SystemError, not a trap */
static PyObject *four_params(PyObject *a, PyObject *b, PyObject *c, PyObject *d) { Py_RETURN_NONE; }

typedef struct {
  PyObject_HEAD
  long value;
} Box;

/* a getter and a setter without the closure */
static PyObject *box_get(Box *self) { return PyLong_FromLong(self->value); }
static int box_set(Box *self, PyObject *value) {
  if (!value) {
    PyErr_SetString(PyExc_TypeError, "value cannot be deleted");
    return -1;
  }
  long v = PyLong_AsLong(value);
  if (v == -1 && PyErr_Occurred()) return -1;
  self->value = v;
  return 0;
}
/* a getter as declared, with its closure */
static PyObject *box_scaled(Box *self, void *closure) { return PyLong_FromLong(self->value * (long)(intptr_t)closure); }
/* methods: METH_NOARGS taking only self, METH_O as declared */
static PyObject *box_bump(Box *self) { return PyLong_FromLong(++self->value); }
static PyObject *box_add(Box *self, PyObject *arg) {
  long v = PyLong_AsLong(arg);
  if (v == -1 && PyErr_Occurred()) return NULL;
  return PyLong_FromLong(self->value += v);
}

static PyMethodDef box_methods[] = {
  {"bump", (PyCFunction)box_bump, METH_NOARGS, NULL},
  {"add", (PyCFunction)box_add, METH_O, NULL},
  {NULL, NULL, 0, NULL},
};

static PyGetSetDef box_getset[] = {
  {"value", (getter)box_get, (setter)box_set, NULL, NULL},
  {"tripled", (getter)box_scaled, NULL, NULL, (void *)(intptr_t)3},
  {NULL, NULL, NULL, NULL, NULL},
};

static PyTypeObject BoxType = {
  PyVarObject_HEAD_INIT(NULL, 0)
  .tp_name = "fpcast.Box",
  .tp_basicsize = sizeof(Box),
  .tp_flags = Py_TPFLAGS_DEFAULT,
  .tp_new = PyType_GenericNew,
  .tp_methods = box_methods,
  .tp_getset = box_getset,
};

static PyMethodDef methods[] = {
  {"one_param", (PyCFunction)one_param, METH_NOARGS, NULL},
  {"in_thread", (PyCFunction)in_thread, METH_NOARGS, NULL},
  {"no_params", (PyCFunction)no_params, METH_NOARGS, NULL},
  {"exact", exact, METH_O, NULL},
  {"no_kwargs", (PyCFunction)no_kwargs, METH_VARARGS | METH_KEYWORDS, NULL},
  {"extra_kwargs", (PyCFunction)extra_kwargs, METH_VARARGS, NULL},
  {"four_params", (PyCFunction)four_params, METH_NOARGS, NULL},
  {NULL, NULL, 0, NULL},
};

static struct PyModuleDef module = {PyModuleDef_HEAD_INIT, "fpcast", NULL, -1, methods};

PyMODINIT_FUNC PyInit_fpcast(void) {
  if (PyType_Ready(&BoxType) < 0) return NULL;
  PyObject *m = PyModule_Create(&module);
  if (!m) return NULL;
  Py_INCREF(&BoxType);
  PyModule_AddObject(m, "Box", (PyObject *)&BoxType);
  return m;
}

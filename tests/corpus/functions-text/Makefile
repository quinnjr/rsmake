L = foo.c bar.c baz.h
all:
	echo "[$(subst .c,.o,$(L))]"
	echo "[$(patsubst %.c,%.o,$(L))]"
	echo "[$(findstring bar,$(L))]"
	echo "[$(filter %.c,$(L))]"
	echo "[$(filter-out %.c,$(L))]"
	echo "[$(sort c b a b)]"
	echo "[$(subst a,b,c,d)]"

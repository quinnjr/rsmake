L = src/a.c b.c dir.d/x
all:
	echo "[$(dir $(L))]"
	echo "[$(notdir $(L))]"
	echo "[$(suffix $(L))]"
	echo "[$(basename $(L))]"
	echo "[$(addprefix o/,$(L))] [$(addsuffix .bak,$(L))]"
	echo "[$(join a b,1 2 3)]"

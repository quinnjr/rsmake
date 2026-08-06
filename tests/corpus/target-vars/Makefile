CFLAGS = base
all: child
	echo "all CFLAGS=$(CFLAGS)"
child: CFLAGS += extra
child: grandchild
	echo "child CFLAGS=$(CFLAGS)"
grandchild:
	echo "grandchild CFLAGS=$(CFLAGS)"

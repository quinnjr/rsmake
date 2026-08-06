define GREET
echo hello $(1)
endef
define RULE
extra:
	echo generated
endef
$(eval $(RULE))
all: extra
	$(call GREET,world)

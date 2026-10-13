ROOT := ../..
LIB  := $(ROOT)/target/release
UNAME := $(shell uname)
ifeq ($(UNAME),Darwin)
  LDLIBS := -framework IOKit -framework CoreFoundation
else
  LDLIBS := -lpthread -ldl -lm
endif

stream: stream.c $(LIB)/libddram.a
	$(CC) -O2 -Wall -Wextra -I$(ROOT)/crates/ddram/include -o $@ stream.c $(LIB)/libddram.a $(LDLIBS)

$(LIB)/libddram.a:
	cargo build --release -p ddram --manifest-path $(ROOT)/Cargo.toml

clean:
	rm -f stream

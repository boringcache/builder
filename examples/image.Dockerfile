FROM alpine:3.21
RUN printf 'hello from boringbuilder\n' > /hello.txt
CMD ["cat", "/hello.txt"]

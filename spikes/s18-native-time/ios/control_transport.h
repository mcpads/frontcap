#pragma once
// Simulator-only loopback transport. Each connection carries one length-prefixed JSON object.
#import <Foundation/Foundation.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <unistd.h>
#include <errno.h>

static BOOL control_socket_options(int fd) {
  int yes = 1;
  struct timeval timeout = {.tv_sec = 10};
  return !setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &yes, sizeof(yes)) &&
    !setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) &&
    !setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout));
}
static NSDictionary *control_receive_bounded(int fd, NSUInteger limit) {
  uint32_t length;
  if (recv(fd, &length, sizeof(length), MSG_WAITALL) != sizeof(length)) return nil;
  length = ntohl(length);
  if (!length || length > limit) return nil;
  NSMutableData *data = [NSMutableData dataWithLength:length];
  if (recv(fd, data.mutableBytes, length, MSG_WAITALL) != length) return nil;
  id value = [NSJSONSerialization JSONObjectWithData:data options:0 error:NULL];
  return [value isKindOfClass:NSDictionary.class] ? value : nil;
}
static NSDictionary *control_receive(int fd) { return control_receive_bounded(fd, 8192); }
static BOOL control_send_bounded(int fd, NSDictionary *value, NSUInteger limit) {
  NSData *data = [NSJSONSerialization dataWithJSONObject:value options:0 error:NULL];
  if (!data || !data.length || data.length > limit) return NO;
  uint32_t length = htonl((uint32_t)data.length);
  NSMutableData *frame = [NSMutableData dataWithBytes:&length length:sizeof(length)];
  [frame appendData:data];
  const unsigned char *bytes = frame.bytes;
  NSUInteger offset = 0;
  while (offset < frame.length) {
    ssize_t written = send(fd, bytes + offset, frame.length - offset, 0);
    if (written < 0 && errno == EINTR) continue;
    if (written <= 0) return NO;
    offset += (NSUInteger)written;
  }
  return YES;
}

static BOOL control_send(int fd, NSDictionary *value) {
  return control_send_bounded(fd, value, 8192);
}
